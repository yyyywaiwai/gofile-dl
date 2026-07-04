#!/usr/bin/env node

import crypto from "node:crypto";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { existsSync } from "node:fs";
import { mkdir, rename, unlink } from "node:fs/promises";
import { createWriteStream } from "node:fs";
import http from "node:http";
import https from "node:https";
import { createRequire } from "node:module";
import path from "node:path";
import process from "node:process";
import * as readline from "node:readline";
import { finished } from "node:stream/promises";
import { fileURLToPath } from "node:url";
import { ProxyAgent } from "proxy-agent";

const require = createRequire(import.meta.url);
const packageJson = require("../package.json");

export const USER_AGENT =
  "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36";

const MAX_REDIRECTS = 10;
const DOWNLOAD_PROGRESS_INTERVAL_MS = 5_000;
const PROGRESS_BAR_WIDTH = 28;
const PROGRESS_RENDER_INTERVAL_MS = 120;
const API_SERVER = "api";
const BROWSER_LANGUAGE = "en-US";
const WEBSITE_TOKEN_SALT = "9844d94d963d30";

class UsageError extends Error {}

export function extractContentId(input) {
  const trimmed = String(input ?? "").trim();
  if (!trimmed) {
    throw new Error("content id is empty");
  }

  try {
    const url = new URL(trimmed);
    const segments = url.pathname.split("/").filter(Boolean);
    const marker = segments.indexOf("d");
    if (marker >= 0 && segments[marker + 1]) {
      return segments[marker + 1];
    }
    throw new Error("could not find /d/<id> in Gofile URL");
  } catch (error) {
    if (error instanceof TypeError) {
      return trimmed;
    }
    throw error;
  }
}

export function sanitizeComponent(name) {
  const sanitized = String(name ?? "")
    .replace(/[\/\\:*?"<>|\0]/g, "_")
    .replace(/[\u0000-\u001f\u007f]/g, "_")
    .trim()
    .replace(/^\.+|\.+$/g, "");
  return sanitized || "_";
}

export function formatBytes(bytes) {
  const value = Number(bytes) || 0;
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let scaled = value;
  let unit = 0;

  while (scaled >= 1024 && unit < units.length - 1) {
    scaled /= 1024;
    unit += 1;
  }

  return unit === 0 ? `${value} ${units[unit]}` : `${scaled.toFixed(1)} ${units[unit]}`;
}

export function generateWebsiteToken(accountToken, now = Date.now()) {
  const period = Math.floor(now / 1000 / 14400).toString();
  return crypto
    .createHash("sha256")
    .update(`${USER_AGENT}::${BROWSER_LANGUAGE}::${accountToken}::${period}::${WEBSITE_TOKEN_SALT}`)
    .digest("hex");
}

export function hashGofilePassword(password) {
  return crypto.createHash("sha256").update(String(password)).digest("hex");
}

function truncateText(value, maxLength) {
  const chars = Array.from(String(value));
  if (chars.length <= maxLength) return String(value);
  if (maxLength <= 1) return chars.slice(0, maxLength).join("");
  return `${chars.slice(0, maxLength - 1).join("")}…`;
}

export function renderProgressLine(progress, columns = 100) {
  const total = Number(progress.total) || 0;
  const downloaded = Number(progress.downloaded) || 0;
  const ratio = total > 0 ? Math.min(downloaded / total, 1) : 0;
  const filled = total > 0 ? Math.round(ratio * PROGRESS_BAR_WIDTH) : 0;
  const bar = `${"#".repeat(filled)}${"-".repeat(PROGRESS_BAR_WIDTH - filled)}`;
  const percent = total > 0 ? `${String(Math.floor(ratio * 100)).padStart(3)}%` : " --%";
  const bytes = total > 0 ? `${formatBytes(downloaded)}/${formatBytes(total)}` : formatBytes(downloaded);
  const fixedWidth = percent.length + bar.length + bytes.length + 5;
  const nameWidth = Math.max(12, columns - fixedWidth - 1);
  const name = truncateText(progress.relativePath, nameWidth);
  return `${percent} [${bar}] ${bytes} ${name}`;
}

class DownloadProgress {
  constructor({ quiet, stream = process.stderr }) {
    this.quiet = quiet;
    this.stream = stream;
    this.enabled = !quiet && Boolean(stream.isTTY);
    this.active = new Map();
    this.renderedLines = 0;
    this.lastRenderAt = 0;
  }

  log(message) {
    if (this.quiet) return;
    if (!this.enabled) {
      logStatus(false, message);
      return;
    }
    this.clear();
    this.stream.write(`[gofile-dl] ${message}\n`);
    this.render(true);
  }

  start(item) {
    if (this.quiet) return;
    if (!this.enabled) {
      logStatus(false, `downloading ${item.relativePath}`);
      return;
    }
    this.active.set(item.relativePath, {
      relativePath: item.relativePath,
      downloaded: 0,
      total: item.size ?? 0,
      lastLogAt: 0,
    });
    this.render(true);
  }

  update(item, downloaded, total) {
    if (this.quiet) return;

    if (!this.enabled) {
      const key = item.relativePath;
      const state = this.active.get(key) ?? { lastLogAt: 0 };
      const now = Date.now();
      if (now - state.lastLogAt >= DOWNLOAD_PROGRESS_INTERVAL_MS) {
        state.lastLogAt = now;
        this.active.set(key, state);
        const suffix = total > 0 ? ` / ${formatBytes(total)}` : "";
        logStatus(false, `downloading ${item.relativePath}: ${formatBytes(downloaded)}${suffix}`);
      }
      return;
    }

    const state =
      this.active.get(item.relativePath) ??
      {
        relativePath: item.relativePath,
        downloaded: 0,
        total: item.size ?? 0,
      };
    state.downloaded = downloaded;
    state.total = total;
    this.active.set(item.relativePath, state);
    this.render();
  }

  finish(item) {
    if (this.quiet) return;
    this.active.delete(item.relativePath);
    this.log(`done ${item.relativePath}`);
  }

  remove(item) {
    if (this.quiet || !this.enabled) return;
    this.active.delete(item.relativePath);
    this.render(true);
  }

  clear() {
    if (!this.enabled || this.renderedLines === 0) return;

    readline.moveCursor(this.stream, 0, -this.renderedLines);
    for (let i = 0; i < this.renderedLines; i += 1) {
      readline.clearLine(this.stream, 0);
      readline.moveCursor(this.stream, 0, 1);
    }
    readline.moveCursor(this.stream, 0, -this.renderedLines);
    this.renderedLines = 0;
  }

  render(force = false) {
    if (!this.enabled) return;

    const now = Date.now();
    if (!force && now - this.lastRenderAt < PROGRESS_RENDER_INTERVAL_MS) return;
    this.lastRenderAt = now;

    this.clear();
    const columns = Math.max(60, Number(this.stream.columns) || 100);
    const lines = Array.from(this.active.values()).map((state) =>
      renderProgressLine(state, columns),
    );
    if (!lines.length) return;

    this.stream.write(`${lines.join("\n")}\n`);
    this.renderedLines = lines.length;
  }
}

export function listDownloadsFromContent(root, rootPath, quiet = true) {
  const items = [];
  collectDownloads(root, rootPath, items, quiet);
  return items;
}

function collectDownloads(content, basePath, items, quiet) {
  if (content?.type === "file") {
    if (content.link) {
      logStatus(quiet, `queued file ${basePath}`);
      items.push({
        url: content.link,
        relativePath: basePath,
        size: Number.isFinite(Number(content.size)) ? Number(content.size) : null,
      });
    }
    return;
  }

  for (const [id, child] of Object.entries(content?.children ?? {})) {
    const name = child?.name?.trim() ? sanitizeComponent(child.name) : sanitizeComponent(id);
    collectDownloads(child, path.join(basePath, name), items, quiet);
  }
}

export function parseArgs(argv) {
  const args = {
    urlOrId: null,
    output: ".",
    password: null,
    jobs: 4,
    overwrite: false,
    dryRun: false,
    quiet: false,
    proxy: process.env.GOFILE_PROXY || null,
    scrapeScript: process.env.GOFILE_SCRAPE_SCRIPT || null,
    help: false,
    version: false,
  };

  const takeValue = (tokens, index, name) => {
    const value = tokens[index + 1];
    if (!value || value.startsWith("-")) {
      throw new UsageError(`${name} requires a value`);
    }
    return value;
  };

  for (let i = 0; i < argv.length; i += 1) {
    const token = argv[i];
    const [flag, inlineValue] = token.startsWith("--") ? token.split(/=(.*)/s, 2) : [token, null];

    switch (flag) {
      case "-h":
      case "--help":
        args.help = true;
        break;
      case "-V":
      case "--version":
        args.version = true;
        break;
      case "-o":
      case "--output":
        args.output = inlineValue ?? takeValue(argv, i++, flag);
        break;
      case "-p":
      case "--password":
        args.password = inlineValue ?? takeValue(argv, i++, flag);
        break;
      case "-j":
      case "--jobs":
        args.jobs = Number.parseInt(inlineValue ?? takeValue(argv, i++, flag), 10);
        break;
      case "-q":
      case "--quiet":
        args.quiet = true;
        break;
      case "--overwrite":
        args.overwrite = true;
        break;
      case "--dry-run":
        args.dryRun = true;
        break;
      case "--proxy":
        args.proxy = inlineValue ?? takeValue(argv, i++, flag);
        break;
      case "--scrape-script":
        args.scrapeScript = inlineValue ?? takeValue(argv, i++, flag);
        break;
      default:
        if (token.startsWith("-")) {
          throw new UsageError(`unknown option ${token}`);
        }
        if (args.urlOrId) {
          throw new UsageError(`unexpected argument ${token}`);
        }
        args.urlOrId = token;
        break;
    }
  }

  if (!Number.isInteger(args.jobs) || args.jobs < 1) {
    throw new UsageError("--jobs must be a positive integer");
  }

  if (!args.help && !args.version && !args.urlOrId) {
    throw new UsageError("missing Gofile URL or content id");
  }

  return args;
}

function usage() {
  return `gofile-dl ${packageJson.version}

Download every file from a Gofile folder using the public share page.

Usage:
  gofile-dl [options] <gofile-url-or-content-id>

Options:
  -o, --output <dir>          Output directory (default: .)
  -p, --password <password>   Folder password
  -j, --jobs <count>          Simultaneous file downloads (default: 4)
      --overwrite             Overwrite files that already exist
      --dry-run               Print files without downloading
  -q, --quiet                 Suppress progress logs on stderr
      --proxy <url>           HTTP(S) or SOCKS proxy for downloads
      --scrape-script <path>  External scraper script returning JSON
  -h, --help                  Show help
  -V, --version               Show version

Environment:
  GOFILE_PROXY, HTTPS_PROXY, ALL_PROXY, GOFILE_SCRAPE_SCRIPT
`;
}

export function normalizeProxyUrl(value) {
  const proxy = String(value ?? "").trim();
  if (!proxy) return null;
  if (/^[a-z][a-z0-9+.-]*:\/\//i.test(proxy)) return proxy;
  return `http://${proxy}`;
}

function resolveProxyUrl(cliProxy) {
  const explicit = String(cliProxy ?? "").trim();
  if (explicit) return normalizeProxyUrl(explicit);

  for (const key of ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]) {
    const value = String(process.env[key] ?? "").trim();
    if (value) return normalizeProxyUrl(value);
  }
  return null;
}

function logStatus(quiet, message) {
  if (!quiet) {
    process.stderr.write(`[gofile-dl] ${message}\n`);
  }
}

async function fetchJson(url, { method = "GET", headers = {}, body = null } = {}) {
  const response = await fetch(url, {
    method,
    headers: {
      "User-Agent": USER_AGENT,
      Referer: "https://gofile.io/",
      ...headers,
    },
    body,
  });
  const text = await response.text();
  let json;
  try {
    json = text ? JSON.parse(text) : {};
  } catch (error) {
    throw new Error(`invalid JSON from ${url}: ${error.message}`);
  }
  if (!response.ok) {
    throw new Error(`http-${response.status} from ${url}: ${json.status ?? text}`);
  }
  return json;
}

async function createGuestWebSession(quiet) {
  logStatus(quiet, "creating guest Gofile web session");
  const account = await fetchJson(`https://${API_SERVER}.gofile.io/accounts`, {
    method: "POST",
  });
  if (account.status !== "ok" || !account.data?.token) {
    throw new Error(`failed to create guest account: ${account.status ?? "missing token"}`);
  }

  const token = account.data.token;
  const website = await fetchJson(`https://${API_SERVER}.gofile.io/accounts/website`, {
    headers: { Authorization: `Bearer ${token}` },
  });
  if (website.status !== "ok") {
    throw new Error(`failed to create website session: ${website.status}`);
  }

  return token;
}

async function getContentDirect(contentId, { token, passwordHash, page = 1, pageSize = 1000 }) {
  const url = new URL(`https://${API_SERVER}.gofile.io/contents/${contentId}`);
  url.search = new URLSearchParams({
    contentFilter: "",
    page: String(page),
    pageSize: String(pageSize),
    sortField: "name",
    sortDirection: "1",
  });
  if (passwordHash) {
    url.searchParams.set("password", passwordHash);
  }

  const result = await fetchJson(url, {
    headers: {
      Authorization: `Bearer ${token}`,
      "X-Website-Token": generateWebsiteToken(token),
      "X-BL": BROWSER_LANGUAGE,
    },
  });

  if (result.status !== "ok") {
    throw new Error(`getContent ${contentId} returned ${result.status}`);
  }
  if (result.data?.password && result.data?.passwordStatus === "passwordWrong") {
    throw new Error(`password is required or wrong for ${contentId}`);
  }
  if (!result.data) {
    throw new Error(`getContent returned no data for ${contentId}`);
  }

  return result.data;
}

export async function scrapeShareTree({
  shareUrl,
  password = null,
  scrapeScript = null,
  quiet = false,
}) {
  if (scrapeScript) {
    return runExternalScraper({ scrapeScript, shareUrl, password, quiet });
  }

  const contentId = extractContentId(shareUrl);
  const token = await createGuestWebSession(quiet);
  const passwordHash = password ? hashGofilePassword(password) : null;

  async function fetchFolderAll(id) {
    const pageSize = 1000;
    let pageNum = 1;
    let merged = null;

    while (true) {
      logStatus(quiet, `fetching folder ${id} page ${pageNum}`);
      const chunk = await getContentDirect(id, {
        token,
        passwordHash,
        page: pageNum,
        pageSize,
      });
      const childCount = Object.keys(chunk.children || {}).length;
      if (!merged) {
        merged = chunk;
      } else {
        Object.assign(merged.children, chunk.children);
      }
      if (childCount < pageSize) break;
      pageNum += 1;
    }

    return merged;
  }

  async function walkFolder(folder) {
    const out = {
      id: folder.id,
      code: folder.code,
      name: folder.name,
      type: folder.type,
      size: folder.size,
      link: folder.link,
      children: {},
    };

    for (const [childId, child] of Object.entries(folder.children || {})) {
      if (child.type === "folder") {
        const nestedId = child.id || child.code || childId;
        const nested = await fetchFolderAll(nestedId);
        out.children[childId] = await walkFolder(nested);
      } else {
        out.children[childId] = child;
      }
    }

    return out;
  }

  const root = await fetchFolderAll(contentId);
  return {
    token,
    root: await walkFolder(root),
  };
}

async function runExternalScraper({ scrapeScript, shareUrl, password, quiet }) {
  const script = path.resolve(scrapeScript);
  if (!existsSync(script)) {
    throw new Error(`scrape script not found at ${script}`);
  }

  logStatus(quiet, `running ${script}`);
  const env = { ...process.env };
  const child = spawn(process.execPath, [script, shareUrl, ...(password ? [password] : [])], {
    env,
    stdio: ["ignore", "pipe", "pipe"],
  });

  let stdout = "";
  let stderr = "";
  child.stdout.setEncoding("utf8");
  child.stderr.setEncoding("utf8");
  child.stdout.on("data", (chunk) => {
    stdout += chunk;
  });
  child.stderr.on("data", (chunk) => {
    stderr += chunk;
  });

  const [code] = await Promise.race([
    once(child, "close"),
    once(child, "error").then(([error]) => {
      throw error;
    }),
  ]);
  if (code !== 0) {
    throw new Error(`share page scrape failed (exit ${code})\nstdout:\n${stdout}\nstderr:\n${stderr}`);
  }

  try {
    return JSON.parse(stdout.trim());
  } catch (error) {
    throw new Error(`failed to parse scrape script JSON: ${error.message}`);
  }
}

async function requestWithRedirects(url, { token, agent }, redirectCount = 0) {
  const requestUrl = new URL(url);
  const transport = requestUrl.protocol === "http:" ? http : https;
  const headers = {
    "User-Agent": USER_AGENT,
    Referer: "https://gofile.io/",
  };
  if (token) {
    headers.Cookie = `accountToken=${token}`;
  }

  return new Promise((resolve, reject) => {
    const request = transport.get(requestUrl, { headers, agent }, (response) => {
      const statusCode = response.statusCode ?? 0;
      const location = response.headers.location;
      if ([301, 302, 303, 307, 308].includes(statusCode) && location) {
        response.resume();
        if (redirectCount >= MAX_REDIRECTS) {
          reject(new Error(`too many redirects while downloading ${url}`));
          return;
        }
        resolve(
          requestWithRedirects(new URL(location, requestUrl).toString(), { token, agent }, redirectCount + 1),
        );
        return;
      }

      if (statusCode < 200 || statusCode >= 300) {
        response.resume();
        reject(new Error(`download request failed with http-${statusCode}`));
        return;
      }
      resolve(response);
    });

    request.on("error", reject);
  });
}

async function writeResponseToFile(response, destination, item, progress) {
  const file = createWriteStream(destination);
  let downloaded = 0;
  const total = Number(response.headers["content-length"] ?? item.size ?? 0);
  progress.update(item, downloaded, total);

  try {
    for await (const chunk of response) {
      downloaded += chunk.length;
      if (!file.write(chunk)) {
        await once(file, "drain");
      }
      progress.update(item, downloaded, total);
    }
  } finally {
    file.end();
  }

  await finished(file);
}

async function downloadOne({ item, outputRoot, token, overwrite, agent, progress }) {
  const destination = path.join(outputRoot, item.relativePath);
  if (!overwrite && existsSync(destination)) {
    progress.log(`skip existing: ${destination}`);
    return;
  }

  await mkdir(path.dirname(destination), { recursive: true });
  const tmpDestination = `${destination}.part`;

  progress.start(item);
  try {
    const response = await requestWithRedirects(item.url, { token, agent });
    await writeResponseToFile(response, tmpDestination, item, progress);
    await rename(tmpDestination, destination);
    progress.finish(item);
  } catch (error) {
    progress.remove(item);
    await unlink(tmpDestination).catch(() => {});
    throw error;
  }
}

async function runPool(items, jobs, worker) {
  let index = 0;
  const failures = [];

  async function loop() {
    while (index < items.length) {
      const item = items[index];
      index += 1;
      try {
        await worker(item);
      } catch (error) {
        failures.push({ item, error });
      }
    }
  }

  const workers = Array.from({ length: Math.min(jobs, items.length) }, () => loop());
  await Promise.all(workers);
  return failures;
}

export async function runCli(argv = process.argv.slice(2)) {
  const args = parseArgs(argv);
  if (args.help) {
    process.stdout.write(usage());
    return;
  }
  if (args.version) {
    process.stdout.write(`${packageJson.version}\n`);
    return;
  }

  const contentId = extractContentId(args.urlOrId);
  const shareUrl = `https://gofile.io/d/${contentId}`;
  const proxyUrl = resolveProxyUrl(args.proxy);
  const agent = proxyUrl ? new ProxyAgent({ getProxyForUrl: () => proxyUrl }) : undefined;

  if (proxyUrl) {
    logStatus(args.quiet, `using proxy ${proxyUrl} for downloads`);
  }

  const payload = await scrapeShareTree({
    shareUrl,
    password: args.password,
    scrapeScript: args.scrapeScript,
    quiet: args.quiet,
  });

  const rootName = payload.root?.name?.trim()
    ? sanitizeComponent(payload.root.name)
    : sanitizeComponent(contentId);
  const items = listDownloadsFromContent(payload.root, rootName, args.quiet);

  if (!items.length) {
    throw new Error("no downloadable files found in this Gofile content");
  }

  const totalSize = items.reduce((sum, item) => sum + (item.size ?? 0), 0);
  logStatus(
    args.quiet,
    `found ${items.length} file(s), total known size ${formatBytes(totalSize)}`,
  );

  if (args.dryRun) {
    for (const item of items) {
      process.stdout.write(`${item.relativePath}\n`);
    }
    process.stdout.write(`${items.length} file(s)\n`);
    return;
  }

  await mkdir(args.output, { recursive: true });
  logStatus(args.quiet, `starting downloads with ${args.jobs} job(s) into ${args.output}`);
  const progress = new DownloadProgress({ quiet: args.quiet });
  const failures = await runPool(items, args.jobs, (item) =>
    downloadOne({
      item,
      outputRoot: args.output,
      token: payload.token,
      overwrite: args.overwrite,
      agent,
      progress,
    }),
  );

  progress.clear();
  if (failures.length) {
    for (const { item, error } of failures) {
      process.stderr.write(`failed: ${item.relativePath}: ${error.message}\n`);
    }
    throw new Error(`${failures.length} download(s) failed`);
  }

  process.stdout.write("download complete\n");
}

function isCliEntrypoint() {
  const entry = process.argv[1];
  if (!entry) return false;
  const resolved = path.resolve(entry);
  const self = fileURLToPath(import.meta.url);
  if (resolved === self) return true;
  const base = path.basename(resolved);
  return base === "gofile-dl" || base === "gofile-dl.mjs" || base === "gofile-dl.cmd";
}

if (isCliEntrypoint()) {
  runCli().catch((error) => {
    if (error instanceof UsageError) {
      process.stderr.write(`${error.message}\n\n${usage()}`);
      process.exitCode = 2;
      return;
    }
    process.stderr.write(`error: ${error.message}\n`);
    process.exitCode = 1;
  });
}
