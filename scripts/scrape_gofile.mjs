#!/usr/bin/env node

import { scrapeShareTree } from "../src/gofile-dl.mjs";

const shareUrl = process.argv[2];
const password = process.argv[3] || "";

if (!shareUrl) {
  process.stderr.write("usage: scrape_gofile.mjs <share-url> [password]\n");
  process.exit(2);
}

try {
  const payload = await scrapeShareTree({
    shareUrl,
    password,
    quiet: true,
  });
  process.stdout.write(JSON.stringify(payload));
} catch (error) {
  process.stderr.write(`${error?.stack || String(error)}\n`);
  process.exit(1);
}
