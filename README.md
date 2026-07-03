# gofile-dl

Download every file from a public Gofile folder share.

Requires Node.js 20+.

## Run without cloning

After the package is published to npm:

```bash
npx gofile-dl --help
npx gofile-dl -o ./downloads https://gofile.io/d/YOUR_ID
```

## Publish to npm (maintainers)

```bash
npm login
npm publish
```

Then anyone can use `npx gofile-dl` from the registry.

## Development

```bash
npm install
npm test
npm run gofile-dl -- --help
```