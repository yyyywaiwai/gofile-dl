use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use futures_util::{StreamExt, stream};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use reqwest::{Client, header};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::{
    fs::{self, File},
    io::AsyncWriteExt,
    time::{Duration, sleep},
};
use url::Url;

const API_SERVER: &str = "api";
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36";
const BROWSER_LANGUAGE: &str = "en-US";
const WEBSITE_TOKEN_SALT: &str = "9844d94d963d30";
const PAGE_SIZE: usize = 1000;
const SCRAPE_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "Download every file from a Gofile folder by mimicking the Gofile web page"
)]
struct Args {
    /// Gofile folder URL such as https://gofile.io/d/r3dUsW, or a raw content id.
    url_or_id: String,

    /// Output directory.
    #[arg(short, long, default_value = ".")]
    output: PathBuf,

    /// Folder password, if the Gofile link is password protected.
    #[arg(short, long)]
    password: Option<String>,

    /// Reuse a Gofile web account token instead of creating a new guest session.
    #[arg(long, env = "GOFILE_WEB_TOKEN")]
    web_token: Option<String>,

    /// Number of simultaneous file downloads.
    #[arg(short = 'j', long, default_value_t = 4)]
    jobs: usize,

    /// Overwrite files that already exist.
    #[arg(long)]
    overwrite: bool,

    /// Print the files that would be downloaded without writing them.
    #[arg(long)]
    dry_run: bool,

    /// Suppress progress logs on stderr.
    #[arg(short, long)]
    quiet: bool,
}

#[derive(Debug, Deserialize)]
struct WebResponse<T> {
    status: String,
    #[serde(default)]
    data: Option<T>,
}

#[derive(Debug, Default, Deserialize)]
struct GuestAccount {
    token: String,
}

#[derive(Debug, Default, Deserialize)]
struct Content {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    link: Option<String>,
    #[serde(default)]
    children: BTreeMap<String, Content>,
    #[serde(rename = "canAccess", default)]
    can_access: Option<bool>,
    #[serde(rename = "passwordStatus", default)]
    password_status: Option<String>,
}

#[derive(Clone, Debug)]
struct DownloadItem {
    url: String,
    relative_path: PathBuf,
    size: Option<u64>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let content_id = extract_content_id(&args.url_or_id)?;
    let jobs = args.jobs.max(1);

    let client = build_client()?;
    log_status(
        args.quiet,
        format_args!("opening Gofile page for {content_id}"),
    );
    fetch_share_page(&client, &content_id, args.quiet).await?;
    let session = WebSession::new(resolve_web_token(&client, args.web_token, args.quiet).await?);
    log_status(
        args.quiet,
        format_args!("scraping root folder {content_id}"),
    );
    let root = fetch_folder_all_pages(
        &client,
        &session,
        &content_id,
        args.password.as_deref(),
        args.quiet,
    )
    .await
    .with_context(|| format!("failed to scrape Gofile content {content_id}"))?;

    let root_name = root
        .name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .map(sanitize_component)
        .unwrap_or_else(|| sanitize_component(&content_id));

    let items = list_downloads(
        &client,
        &session,
        root,
        PathBuf::from(root_name),
        args.password.as_deref(),
        args.quiet,
    )
    .await?;

    if items.is_empty() {
        bail!("no downloadable files found in this Gofile content");
    }

    let total_size = items.iter().filter_map(|item| item.size).sum::<u64>();
    log_status(
        args.quiet,
        format_args!(
            "found {} file(s), total known size {}",
            items.len(),
            format_bytes(total_size)
        ),
    );

    if args.dry_run {
        log_status(args.quiet, format_args!("dry run: printing file list only"));
        for item in &items {
            println!("{}", item.relative_path.display());
        }
        println!("{} file(s)", items.len());
        return Ok(());
    }

    fs::create_dir_all(&args.output)
        .await
        .with_context(|| format!("failed to create {}", args.output.display()))?;
    log_status(
        args.quiet,
        format_args!(
            "starting downloads with {jobs} job(s) into {}",
            args.output.display()
        ),
    );

    let multi = Arc::new(MultiProgress::new());
    let style = ProgressStyle::with_template(
        "{spinner:.green} {msg} [{bar:40.cyan/blue}] {bytes}/{total_bytes} {eta}",
    )
    .unwrap()
    .progress_chars("#>-");

    let failures = stream::iter(items)
        .map(|item| {
            let client = client.clone();
            let output = args.output.clone();
            let session = session.clone();
            let multi = Arc::clone(&multi);
            let style = style.clone();
            async move {
                download_one(
                    &client,
                    &item,
                    &output,
                    &session,
                    args.overwrite,
                    &multi,
                    style,
                )
                .await
                .map_err(|err| (item.relative_path.clone(), err))
            }
        })
        .buffer_unordered(jobs)
        .filter_map(|result| async move { result.err() })
        .collect::<Vec<_>>()
        .await;

    if !failures.is_empty() {
        for (path, err) in &failures {
            eprintln!("failed: {}: {err:#}", path.display());
        }
        bail!("{} download(s) failed", failures.len());
    }

    println!("download complete");
    Ok(())
}

fn build_client() -> Result<Client> {
    let mut headers = header::HeaderMap::new();
    headers.insert(
        header::USER_AGENT,
        header::HeaderValue::from_static(USER_AGENT),
    );
    headers.insert(
        header::REFERER,
        header::HeaderValue::from_static("https://gofile.io/"),
    );

    Client::builder()
        .default_headers(headers)
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .context("failed to build HTTP client")
}

#[derive(Clone, Debug)]
struct WebSession {
    token: String,
}

impl WebSession {
    fn new(token: String) -> Self {
        Self { token }
    }

    fn website_token(&self) -> String {
        generate_website_token(&self.token, current_wt_period())
    }
}

async fn fetch_share_page(client: &Client, content_id: &str, quiet: bool) -> Result<()> {
    for attempt in 0..3 {
        log_status(
            quiet,
            format_args!(
                "fetching https://gofile.io/d/{content_id}, attempt {}",
                attempt + 1
            ),
        );

        let result = client
            .get(format!("https://gofile.io/d/{content_id}"))
            .timeout(SCRAPE_REQUEST_TIMEOUT)
            .send()
            .await
            .and_then(|response| response.error_for_status());

        match result {
            Ok(_) => {
                log_status(quiet, format_args!("page opened"));
                return Ok(());
            }
            Err(err) if attempt < 2 => {
                let wait = Duration::from_secs(2u64.pow(attempt + 1));
                log_status(
                    quiet,
                    format_args!("page fetch failed: {err}; retrying in {}s", wait.as_secs()),
                );
                sleep(wait).await;
            }
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("failed to fetch https://gofile.io/d/{content_id}"));
            }
        }
    }

    unreachable!("fetch_share_page retry loop always returns")
}

async fn create_guest_account(client: &Client, quiet: bool) -> Result<GuestAccount> {
    let mut last_status = None;

    for attempt in 0..5 {
        log_status(
            quiet,
            format_args!(
                "creating temporary Gofile web session, attempt {}",
                attempt + 1
            ),
        );
        let response = client
            .post(format!("https://{API_SERVER}.gofile.io/accounts"))
            .timeout(SCRAPE_REQUEST_TIMEOUT)
            .send()
            .await?;

        if response.status().as_u16() == 429 {
            last_status = Some("http-429".to_string());
            let wait = Duration::from_secs(2u64.pow(attempt + 1));
            log_status(
                quiet,
                format_args!(
                    "rate limited while creating session; waiting {}s",
                    wait.as_secs()
                ),
            );
            sleep(wait).await;
            continue;
        }

        let response = response.error_for_status()?;
        let body = response.json::<WebResponse<GuestAccount>>().await?;
        if body.status == "error-rateLimit" {
            last_status = Some(body.status);
            let wait = Duration::from_secs(2u64.pow(attempt + 1));
            log_status(
                quiet,
                format_args!(
                    "rate limited while creating session; waiting {}s",
                    wait.as_secs()
                ),
            );
            sleep(wait).await;
            continue;
        }

        if body.status != "ok" {
            bail!(
                "Gofile web account creation returned status '{}'",
                body.status
            );
        }

        log_status(quiet, format_args!("temporary web session ready"));
        return body
            .data
            .ok_or_else(|| anyhow!("Gofile web account creation returned no data"));
    }

    bail!(
        "Gofile rate limit while creating a web guest account; last status: {}",
        last_status.unwrap_or_else(|| "unknown".to_string())
    )
}

async fn resolve_web_token(
    client: &Client,
    explicit_token: Option<String>,
    quiet: bool,
) -> Result<String> {
    if let Some(token) = explicit_token.filter(|token| !token.trim().is_empty()) {
        log_status(quiet, format_args!("using provided Gofile web token"));
        return Ok(token);
    }

    let cache_path = web_token_cache_path();
    if let Some(path) = &cache_path {
        if let Ok(token) = fs::read_to_string(path).await {
            let token = token.trim();
            if !token.is_empty() {
                log_status(
                    quiet,
                    format_args!("using cached Gofile web token from {}", path.display()),
                );
                return Ok(token.to_string());
            }
        }
    }

    let account = create_guest_account(client, quiet).await?;
    if let Some(path) = &cache_path {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent).await;
        }
        if fs::write(path, &account.token).await.is_ok() {
            log_status(
                quiet,
                format_args!("cached Gofile web token at {}", path.display()),
            );
        }
    }
    Ok(account.token)
}

fn web_token_cache_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".cache").join("gofile-dl").join("web-token"))
}

async fn fetch_folder_all_pages(
    client: &Client,
    session: &WebSession,
    content_id: &str,
    password: Option<&str>,
    quiet: bool,
) -> Result<Content> {
    let mut merged: Option<Content> = None;
    let mut page = 1usize;

    loop {
        let page_content =
            fetch_content_page(client, session, content_id, password, page, quiet).await?;
        let child_count = page_content.children.len();
        log_status(
            quiet,
            format_args!("folder {content_id} page {page}: {child_count} item(s)"),
        );

        if let Some(existing) = &mut merged {
            existing.children.extend(page_content.children);
        } else {
            merged = Some(page_content);
        }

        if child_count < PAGE_SIZE {
            break;
        }
        page += 1;
    }

    merged.ok_or_else(|| anyhow!("Gofile returned no content pages"))
}

async fn fetch_content_page(
    client: &Client,
    session: &WebSession,
    content_id: &str,
    password: Option<&str>,
    page: usize,
    quiet: bool,
) -> Result<Content> {
    let mut last_status = None;

    for attempt in 0..4 {
        log_status(
            quiet,
            format_args!(
                "fetching folder {content_id} page {page}, attempt {}",
                attempt + 1
            ),
        );
        let mut request = client
            .get(format!(
                "https://{API_SERVER}.gofile.io/contents/{content_id}"
            ))
            .query(&[
                ("contentFilter", ""),
                ("page", &page.to_string()),
                ("pageSize", &PAGE_SIZE.to_string()),
                ("sortField", "name"),
                ("sortDirection", "1"),
            ])
            .bearer_auth(&session.token)
            .header("X-Website-Token", session.website_token())
            .header("X-BL", BROWSER_LANGUAGE)
            .header(header::COOKIE, format!("accountToken={}", session.token))
            .timeout(SCRAPE_REQUEST_TIMEOUT);

        if let Some(password) = password.filter(|value| !value.is_empty()) {
            request = request.query(&[("password", password)]);
        }

        let response = request.send().await?;
        if response.status().as_u16() == 429 {
            last_status = Some("http-429".to_string());
            let wait = Duration::from_secs(2u64.pow(attempt + 1));
            log_status(
                quiet,
                format_args!(
                    "rate limited on folder {content_id} page {page}; waiting {}s",
                    wait.as_secs()
                ),
            );
            sleep(wait).await;
            continue;
        }

        let response = response.error_for_status()?;
        let body = response.json::<WebResponse<Content>>().await?;
        if body.status == "error-rateLimit" {
            last_status = Some(body.status);
            let wait = Duration::from_secs(2u64.pow(attempt + 1));
            log_status(
                quiet,
                format_args!(
                    "rate limited on folder {content_id} page {page}; waiting {}s",
                    wait.as_secs()
                ),
            );
            sleep(wait).await;
            continue;
        }

        if body.status != "ok" && body.status != "error-notFound" {
            bail!("Gofile web endpoint returned status '{}'", body.status);
        }

        let content = body
            .data
            .ok_or_else(|| anyhow!("Gofile web endpoint returned no content data"))?;
        if content.can_access == Some(false) {
            if content.password_status.as_deref() == Some("passwordRequired")
                || content.password_status.as_deref() == Some("passwordWrong")
            {
                bail!("this Gofile folder needs a valid --password");
            }
            bail!("this Gofile content is not publicly accessible");
        }

        return Ok(content);
    }

    bail!(
        "Gofile rate limit while scraping content {} page {}; last status: {}",
        content_id,
        page,
        last_status.unwrap_or_else(|| "unknown".to_string())
    )
}

async fn list_downloads(
    client: &Client,
    session: &WebSession,
    root: Content,
    root_path: PathBuf,
    password: Option<&str>,
    quiet: bool,
) -> Result<Vec<DownloadItem>> {
    let mut items = Vec::new();
    let mut stack = vec![(root, root_path)];

    while let Some((content, path)) = stack.pop() {
        if content.kind.as_deref() == Some("file") {
            if let Some(url) = content.link {
                log_status(quiet, format_args!("queued file {}", path.display()));
                items.push(DownloadItem {
                    url,
                    relative_path: path,
                    size: content.size,
                });
            }
            continue;
        }

        for (fallback_id, child) in content.children {
            let name = child
                .name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
                .map(sanitize_component)
                .unwrap_or_else(|| sanitize_component(&fallback_id));
            let child_path = path.join(name);

            if child.kind.as_deref() == Some("folder") {
                let child_id = child
                    .id
                    .as_deref()
                    .or(child.code.as_deref())
                    .unwrap_or(&fallback_id)
                    .to_string();
                log_status(
                    quiet,
                    format_args!("entering folder {} ({child_id})", child_path.display()),
                );
                let full_child =
                    fetch_folder_all_pages(client, session, &child_id, password, quiet).await?;
                stack.push((full_child, child_path));
            } else {
                stack.push((child, child_path));
            }
        }
    }

    Ok(items)
}

async fn download_one(
    client: &Client,
    item: &DownloadItem,
    output_root: &Path,
    session: &WebSession,
    overwrite: bool,
    multi: &MultiProgress,
    style: ProgressStyle,
) -> Result<()> {
    let destination = output_root.join(&item.relative_path);

    if !overwrite && fs::try_exists(&destination).await? {
        eprintln!("[gofile-dl] skip existing: {}", destination.display());
        return Ok(());
    }

    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).await?;
    }

    let progress = multi.add(ProgressBar::new(item.size.unwrap_or(0)));
    progress.set_style(style);
    progress.set_message(item.relative_path.display().to_string());

    let request = client
        .get(&item.url)
        .header(header::COOKIE, format!("accountToken={}", session.token));

    let response = request.send().await?.error_for_status()?;
    if progress.length().unwrap_or(0) == 0 {
        if let Some(length) = response.content_length() {
            progress.set_length(length);
        }
    }

    let tmp_destination = destination.with_extension(format!(
        "{}part",
        destination
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| format!("{ext}."))
            .unwrap_or_default()
    ));

    let mut file = File::create(&tmp_destination).await?;
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk).await?;
        progress.inc(chunk.len() as u64);
    }

    file.flush().await?;
    drop(file);
    fs::rename(&tmp_destination, &destination).await?;
    progress.finish_with_message(format!("done {}", item.relative_path.display()));

    Ok(())
}

#[cfg(test)]
fn collect_downloads(content: &Content, base: PathBuf, items: &mut Vec<DownloadItem>) {
    match content.kind.as_deref() {
        Some("file") => {
            if let Some(url) = &content.link {
                items.push(DownloadItem {
                    url: url.clone(),
                    relative_path: base,
                    size: content.size,
                });
            }
        }
        _ => {
            for (id, child) in &content.children {
                let name = child
                    .name
                    .as_deref()
                    .filter(|name| !name.trim().is_empty())
                    .map(sanitize_component)
                    .unwrap_or_else(|| sanitize_component(id));
                collect_downloads(child, base.join(name), items);
            }
        }
    }
}

fn extract_content_id(input: &str) -> Result<String> {
    if let Ok(url) = Url::parse(input) {
        let mut segments = url
            .path_segments()
            .ok_or_else(|| anyhow!("Gofile URL has no path"))?;
        while let Some(segment) = segments.next() {
            if segment == "d" {
                if let Some(id) = segments.next() {
                    if !id.trim().is_empty() {
                        return Ok(id.to_string());
                    }
                }
            }
        }
        bail!("could not find /d/<id> in Gofile URL");
    }

    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("content id is empty");
    }
    Ok(trimmed.to_string())
}

fn generate_website_token(account_token: &str, period: u64) -> String {
    sha256_hex(&format!(
        "{USER_AGENT}::{BROWSER_LANGUAGE}::{account_token}::{period}::{WEBSITE_TOKEN_SALT}"
    ))
}

fn current_wt_period() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        / 14_400
}

fn sanitize_component(name: &str) -> String {
    let sanitized = name
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            ch if ch.is_control() => '_',
            ch => ch,
        })
        .collect::<String>()
        .trim()
        .trim_matches('.')
        .to_string();

    if sanitized.is_empty() {
        "_".to_string()
    } else {
        sanitized
    }
}

fn sha256_hex(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn log_status(quiet: bool, args: std::fmt::Arguments<'_>) {
    if !quiet {
        eprintln!("[gofile-dl] {args}");
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;

    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_content_id_from_gofile_url() {
        assert_eq!(
            extract_content_id("https://gofile.io/d/r3dUsW").unwrap(),
            "r3dUsW"
        );
    }

    #[test]
    fn accepts_raw_content_id() {
        assert_eq!(extract_content_id("r3dUsW").unwrap(), "r3dUsW");
    }

    #[test]
    fn sanitizes_path_components() {
        assert_eq!(sanitize_component(r#"bad/name:*?"#), "bad_name___");
        assert_eq!(sanitize_component("..."), "_");
    }

    #[test]
    fn generates_browser_website_token() {
        assert_eq!(
            generate_website_token("UhjT8mfpQCuFaTt1PQJIsQ2D5i5O2tlW", 123823),
            "44c2bfadf3c7252b29e49f327fbb5605a6c0f14aab0578ce958db924fbd2c3cd"
        );
    }

    #[test]
    fn flattens_nested_content_tree() {
        let json = r#"{
            "name": "root",
            "type": "folder",
            "children": {
                "folder-id": {
                    "name": "sub/folder",
                    "type": "folder",
                    "children": {
                        "file-id": {
                            "name": "video.mp4",
                            "type": "file",
                            "size": 42,
                            "link": "https://example.com/video.mp4"
                        }
                    }
                }
            }
        }"#;

        let content: Content = serde_json::from_str(json).unwrap();
        let mut items = Vec::new();
        collect_downloads(&content, PathBuf::from("root"), &mut items);

        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].relative_path,
            PathBuf::from("root/sub_folder/video.mp4")
        );
        assert_eq!(items[0].size, Some(42));
    }
}
