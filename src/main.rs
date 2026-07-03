use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use futures_util::{StreamExt, stream};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use reqwest::{Client, header};
use serde::Deserialize;
use tokio::{
    fs::{self, File},
    io::AsyncWriteExt,
    process::Command,
    time::Duration,
};
use url::Url;

const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const SCRAPE_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const DOWNLOAD_REQUEST_TIMEOUT: Duration = Duration::from_secs(0);

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "Download every file from a Gofile folder using the public share page (browser scrape)"
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

    /// HTTP(S) or SOCKS proxy for file downloads (also reads HTTPS_PROXY / ALL_PROXY).
    #[arg(long, env = "GOFILE_PROXY")]
    proxy: Option<String>,

    /// Chrome/Chromium binary used for scraping (also reads CHROME_PATH).
    #[arg(long, env = "CHROME_PATH")]
    chrome_path: Option<PathBuf>,

    /// Node script that loads the share page and calls the site getContent() API in-browser.
    #[arg(long, env = "GOFILE_SCRAPE_SCRIPT")]
    scrape_script: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct ScrapePayload {
    #[serde(default)]
    token: Option<String>,
    root: Content,
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
    let share_url = format!("https://gofile.io/d/{content_id}");
    let jobs = args.jobs.max(1);

    let client = build_client(args.proxy.as_deref())?;
    if let Some(proxy) = resolve_proxy_url(args.proxy.as_deref()) {
        log_status(args.quiet, format_args!("using proxy {proxy} for downloads"));
    }

    log_status(
        args.quiet,
        format_args!("scraping share page {share_url} in headless Chrome"),
    );
    let payload = scrape_share_tree(
        &share_url,
        args.password.as_deref(),
        args.chrome_path.as_deref(),
        args.scrape_script.as_deref(),
        args.quiet,
    )
    .await
    .with_context(|| format!("failed to scrape Gofile share page {share_url}"))?;

    let root = payload.root;
    let root_name = root
        .name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .map(sanitize_component)
        .unwrap_or_else(|| sanitize_component(&content_id));

    let items = list_downloads_from_content(root, PathBuf::from(root_name), args.quiet);

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

    let account_token = payload.token;
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
            let token = account_token.clone();
            let multi = Arc::clone(&multi);
            let style = style.clone();
            async move {
                download_one(
                    &client,
                    &item,
                    &output,
                    token.as_deref(),
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

fn resolve_proxy_url(cli_proxy: Option<&str>) -> Option<String> {
    cli_proxy
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
                .into_iter()
                .find_map(|key| std::env::var(key).ok())
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
}

fn build_client(cli_proxy: Option<&str>) -> Result<Client> {
    let mut headers = header::HeaderMap::new();
    headers.insert(
        header::USER_AGENT,
        header::HeaderValue::from_static(USER_AGENT),
    );
    headers.insert(
        header::REFERER,
        header::HeaderValue::from_static("https://gofile.io/"),
    );

    let mut builder = Client::builder()
        .default_headers(headers)
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::limited(10))
        .tcp_keepalive(Duration::from_secs(60))
        .http1_only();

    if let Some(proxy_url) = resolve_proxy_url(cli_proxy) {
        builder = builder.proxy(
            reqwest::Proxy::all(&proxy_url)
                .with_context(|| format!("invalid proxy URL {proxy_url}"))?,
        );
    }

    builder.build().context("failed to build HTTP client")
}

fn default_scrape_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/scrape_gofile.mjs")
}

async fn scrape_share_tree(
    share_url: &str,
    password: Option<&str>,
    chrome_path: Option<&Path>,
    scrape_script: Option<&Path>,
    quiet: bool,
) -> Result<ScrapePayload> {
    let script = scrape_script
        .map(Path::to_path_buf)
        .unwrap_or_else(default_scrape_script);
    if !script.is_file() {
        bail!(
            "scrape script not found at {}; run npm install in {}",
            script.display(),
            script.parent().unwrap_or(Path::new(".")).display()
        );
    }

    ensure_scrape_dependencies(script.parent().unwrap()).await?;

    let node = which_node()?;
    let mut command = Command::new(node);
    command.arg(&script).arg(share_url);
    if let Some(password) = password.filter(|value| !value.is_empty()) {
        command.arg(password);
    }
    if let Some(chrome) = chrome_path {
        command.env("CHROME_PATH", chrome);
    }

    log_status(quiet, format_args!("running {}", script.display()));
    let output = command
        .output()
        .await
        .with_context(|| format!("failed to run scrape script {}", script.display()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        bail!(
            "share page scrape failed (exit {})\nstdout:\n{stdout}\nstderr:\n{stderr}",
            output.status
        );
    }

    let stdout = String::from_utf8(output.stdout).context("scrape script stdout is not utf-8")?;
    let payload: ScrapePayload =
        serde_json::from_str(stdout.trim()).context("failed to parse scrape script JSON")?;
    Ok(payload)
}

async fn ensure_scrape_dependencies(script_dir: &Path) -> Result<()> {
    let node_modules = script_dir.join("node_modules/puppeteer-core");
    if node_modules.is_dir() {
        return Ok(());
    }

    let npm = which_npm()?;
    let status = Command::new(npm)
        .arg("install")
        .current_dir(script_dir)
        .status()
        .await
        .with_context(|| format!("failed to run npm install in {}", script_dir.display()))?;
    if !status.success() {
        bail!(
            "npm install failed in {}; install puppeteer-core manually",
            script_dir.display()
        );
    }
    Ok(())
}

fn which_node() -> Result<String> {
    std::env::var("NODE")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            for candidate in ["/opt/homebrew/bin/node", "/usr/local/bin/node", "/usr/bin/node"] {
                if Path::new(candidate).is_file() {
                    return Some(candidate.to_string());
                }
            }
            None
        })
        .ok_or_else(|| anyhow!("node not found; install Node.js or set NODE"))
}

fn which_npm() -> Result<String> {
    std::env::var("NPM")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            for candidate in ["/opt/homebrew/bin/npm", "/usr/local/bin/npm", "/usr/bin/npm"] {
                if Path::new(candidate).is_file() {
                    return Some(candidate.to_string());
                }
            }
            None
        })
        .ok_or_else(|| anyhow!("npm not found; install Node.js or set NPM"))
}

fn list_downloads_from_content(
    root: Content,
    root_path: PathBuf,
    quiet: bool,
) -> Vec<DownloadItem> {
    let mut items = Vec::new();
    collect_downloads(&root, root_path, &mut items, quiet);
    items
}

fn collect_downloads(content: &Content, base: PathBuf, items: &mut Vec<DownloadItem>, quiet: bool) {
    match content.kind.as_deref() {
        Some("file") => {
            if let Some(url) = &content.link {
                log_status(quiet, format_args!("queued file {}", base.display()));
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
                collect_downloads(child, base.join(name), items, quiet);
            }
        }
    }
}

async fn download_one(
    client: &Client,
    item: &DownloadItem,
    output_root: &Path,
    account_token: Option<&str>,
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

    let mut request = client.get(&item.url);
    if let Some(token) = account_token.filter(|value| !value.is_empty()) {
        request = request.header(header::COOKIE, format!("accountToken={token}"));
    }
    if DOWNLOAD_REQUEST_TIMEOUT.as_secs() > 0 {
        request = request.timeout(DOWNLOAD_REQUEST_TIMEOUT);
    }

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
        let items = list_downloads_from_content(content, PathBuf::from("root"), true);

        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].relative_path,
            PathBuf::from("root/sub_folder/video.mp4")
        );
        assert_eq!(items[0].size, Some(42));
    }
}