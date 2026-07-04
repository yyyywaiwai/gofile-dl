# gofile-dl

Download every file from a public [Gofile](https://gofile.io/) folder share from the terminal.

- Recursively walks nested folders and preserves relative paths
- Parallel downloads with a live progress bar (TTY)
- Guest web session against Gofile’s public API (no account required for typical shares)
- Optional password-protected folders, proxy, and dry-run listing

**Requirements:** [Node.js](https://nodejs.org/) 20 or newer.

## Quick start

```bash
npx gofile-dl https://gofile.io/d/YOUR_CONTENT_ID
```

Save into a directory (default is the current working directory):

```bash
npx gofile-dl -o ./downloads https://gofile.io/d/YOUR_CONTENT_ID
```

You can pass a full share URL or only the content id (`YOUR_CONTENT_ID`).

## Install

| Method | Command |
| --- | --- |
| One-off run | `npx gofile-dl …` |
| Global CLI | `npm install -g gofile-dl` then `gofile-dl …` |
| From source | `git clone … && cd gofile-dl && npm install && npm run gofile-dl -- …` |

## Usage

```text
gofile-dl [options] <gofile-url-or-content-id>
```

### Options

| Option | Description |
| --- | --- |
| `-o, --output <dir>` | Output directory (default: `.`) |
| `-p, --password <password>` | Folder password when the share is protected |
| `-j, --jobs <count>` | Concurrent file downloads (default: `4`) |
| `--overwrite` | Replace files that already exist (default: skip existing) |
| `--dry-run` | List files and sizes without downloading |
| `-q, --quiet` | No progress output on stderr |
| `--proxy <url>` | HTTP(S) or SOCKS proxy for downloads |
| `--scrape-script <path>` | Use an external script that prints share JSON to stdout |
| `-h, --help` | Show help |
| `-V, --version` | Show version |

### Environment variables

| Variable | Purpose |
| --- | --- |
| `GOFILE_PROXY` | Default proxy URL (overridden by `--proxy`) |
| `HTTPS_PROXY`, `https_proxy`, `ALL_PROXY`, `all_proxy` | Used when no CLI proxy is set |
| `GOFILE_SCRAPE_SCRIPT` | Default path for `--scrape-script` |

### Examples

Password-protected share:

```bash
gofile-dl -o ./out -p 'your-password' https://gofile.io/d/AbCdEf
```

Inspect the file list only:

```bash
gofile-dl --dry-run https://gofile.io/d/AbCdEf
```

Eight parallel downloads through a proxy:

```bash
gofile-dl -j 8 --proxy http://127.0.0.1:7890 -o ./out https://gofile.io/d/AbCdEf
```

## External scrape script

By default, `gofile-dl` talks to Gofile’s API directly. If you need a custom discovery path, point at a Node script that:

1. Receives `shareUrl` and optional `password` as CLI arguments (same as the bundled helper)
2. Writes a single JSON object to stdout: `{ "token": "…", "root": { …folder tree… } }`

Bundled helper (also shipped in the npm package):

```bash
node scripts/scrape_gofile.mjs 'https://gofile.io/d/AbCdEf' > share.json
gofile-dl --scrape-script ./scripts/scrape_gofile.mjs -o ./out https://gofile.io/d/AbCdEf
```

## Browser userscript

For downloading from the share page inside the browser (Tampermonkey / Violentmonkey), see [`scripts/gofile-bulk-download.user.js`](scripts/gofile-bulk-download.user.js). Install the script in your userscript manager and open a `https://gofile.io/d/…` page.

## Development

```bash
npm install
npm test          # node --test
npm run check     # syntax check entrypoints
npm run build     # test + check (runs before publish)
npm run gofile-dl -- --help
```

Tests live under [`test/`](test/); they cover argument parsing, path sanitization, and download orchestration against a local HTTP fixture.

## Publish to npm (maintainers)

```bash
npm login
npm publish
```

`prepublishOnly` runs `npm run build`. Published files are `src/` and `scripts/scrape_gofile.mjs` (see `package.json` `"files"`).

## License

MIT — see [`package.json`](package.json).

## Disclaimer

Use this tool only for content you are allowed to download. Gofile’s terms and rate limits apply. This project is not affiliated with Gofile.