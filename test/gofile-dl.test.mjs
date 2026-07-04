import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import {
  extractContentId,
  formatBytes,
  generateWebsiteToken,
  hashGofilePassword,
  listDownloadsFromContent,
  normalizeProxyUrl,
  parseArgs,
  renderProgressLine,
  sanitizeComponent,
} from "../src/gofile-dl.mjs";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

test("extracts content id from Gofile URL", () => {
  assert.equal(extractContentId("https://gofile.io/d/r3dUsW"), "r3dUsW");
});

test("accepts raw content id", () => {
  assert.equal(extractContentId("r3dUsW"), "r3dUsW");
});

test("sanitizes path components", () => {
  assert.equal(sanitizeComponent("bad/name:*?"), "bad_name___");
  assert.equal(sanitizeComponent("..."), "_");
});

test("formats byte counts", () => {
  assert.equal(formatBytes(0), "0 B");
  assert.equal(formatBytes(42), "42 B");
  assert.equal(formatBytes(1536), "1.5 KiB");
});

test("generates Gofile web request tokens", () => {
  assert.equal(
    hashGofilePassword("secret"),
    "2bb80d537b1da3e38bd30361aa855686bde0eacd7162fef6a25fe97bf527a25b",
  );
  assert.equal(
    generateWebsiteToken("token", 0),
    "d1733d06e5d427b9b697ea49975f007bb6f27d93ff70825eb842a156bbdee5b1",
  );
});

test("renders progress bar lines", () => {
  assert.equal(
    renderProgressLine(
      { relativePath: "root/video.mp4", downloaded: 50, total: 100 },
      80,
    ),
    " 50% [##############--------------] 50 B/100 B root/video.mp4",
  );
});

test("flattens nested content tree", () => {
  const content = {
    name: "root",
    type: "folder",
    children: {
      "folder-id": {
        name: "sub/folder",
        type: "folder",
        children: {
          "file-id": {
            name: "video.mp4",
            type: "file",
            size: 42,
            link: "https://example.com/video.mp4",
          },
        },
      },
    },
  };

  const items = listDownloadsFromContent(content, "root", true);

  assert.equal(items.length, 1);
  assert.equal(items[0].relativePath, "root/sub_folder/video.mp4");
  assert.equal(items[0].size, 42);
});

test("parses CLI flags", () => {
  const args = parseArgs([
    "--output",
    "downloads",
    "--password=secret",
    "--jobs",
    "2",
    "--overwrite",
    "--dry-run",
    "--quiet",
    "https://gofile.io/d/r3dUsW",
  ]);

  assert.equal(args.output, "downloads");
  assert.equal(args.password, "secret");
  assert.equal(args.jobs, 2);
  assert.equal(args.overwrite, true);
  assert.equal(args.dryRun, true);
  assert.equal(args.quiet, true);
  assert.equal(args.urlOrId, "https://gofile.io/d/r3dUsW");
});

test("normalizes proxy host and port shorthand", () => {
  assert.equal(normalizeProxyUrl("103.159.96.195:8080"), "http://103.159.96.195:8080");
  assert.equal(normalizeProxyUrl(" http://127.0.0.1:7890 "), "http://127.0.0.1:7890");
  assert.equal(normalizeProxyUrl("socks5://127.0.0.1:1080"), "socks5://127.0.0.1:1080");
  assert.equal(normalizeProxyUrl(""), null);
});

test("runs dry-run CLI with external scraper JSON", () => {
  const result = spawnSync(
    process.execPath,
    [
      path.join(repoRoot, "src/gofile-dl.mjs"),
      "--dry-run",
      "--quiet",
      "--scrape-script",
      path.join(repoRoot, "fixtures/scrape_fixture.mjs"),
      "r3dUsW",
    ],
    { cwd: repoRoot, encoding: "utf8" },
  );

  assert.equal(result.status, 0, result.stderr);
  assert.equal(result.stdout, "root/sub_folder/video.mp4\n1 file(s)\n");
});

test("downloads file from external scraper JSON", async () => {
  const tempDir = await mkdtemp(path.join(os.tmpdir(), "gofile-dl-test-"));
  const server = http.createServer((request, response) => {
    if (request.url !== "/video.mp4") {
      response.writeHead(404);
      response.end();
      return;
    }
    const body = Buffer.from("fixture video\n");
    response.writeHead(200, {
      "content-length": body.length,
      "content-type": "application/octet-stream",
    });
    response.end(body);
  });

  try {
    server.listen(0, "127.0.0.1");
    await once(server, "listening");
    const { port } = server.address();
    const scraper = path.join(tempDir, "scrape_fixture.mjs");
    await writeFile(
      scraper,
      `process.stdout.write(JSON.stringify({
        token: null,
        root: {
          name: "root",
          type: "folder",
          children: {
            file: {
              name: "video.mp4",
              type: "file",
              size: 14,
              link: "http://127.0.0.1:${port}/video.mp4"
            }
          }
        }
      }));\n`,
    );

    const result = await runNode([
      path.join(repoRoot, "src/gofile-dl.mjs"),
      "--quiet",
      "--scrape-script",
      scraper,
      "--output",
      tempDir,
      "fixture",
    ]);

    assert.equal(result.status, 0, result.stderr);
    assert.equal(result.stdout, "download complete\n");
    assert.equal(await readFile(path.join(tempDir, "root/video.mp4"), "utf8"), "fixture video\n");
  } finally {
    server.close();
    await rm(tempDir, { recursive: true, force: true });
  }
});

function runNode(args) {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, args, { cwd: repoRoot, stdio: ["ignore", "pipe", "pipe"] });
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
    child.on("error", reject);
    child.on("close", (status) => {
      resolve({ status, stdout, stderr });
    });
  });
}
