import puppeteer from "puppeteer-core";
import { existsSync } from "node:fs";

const shareUrl = process.argv[2];
const password = process.argv[3] || "";
const chromeCandidates = [
  process.env.CHROME_PATH,
  "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  "/Applications/Chromium.app/Contents/MacOS/Chromium",
  "/usr/bin/google-chrome",
  "/usr/bin/chromium",
].filter(Boolean);

const executablePath = chromeCandidates.find((path) => existsSync(path));
if (!shareUrl) {
  console.error("usage: scrape_gofile.mjs <share-url> [password]");
  process.exit(2);
}
if (!executablePath) {
  console.error("no Chrome/Chromium binary found; set CHROME_PATH");
  process.exit(2);
}

const browser = await puppeteer.launch({
  headless: true,
  executablePath,
  args: ["--no-sandbox", "--disable-dev-shm-usage"],
});

try {
  const page = await browser.newPage();
  await page.setUserAgent(
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36",
  );
  await page.goto(shareUrl, { waitUntil: "networkidle2", timeout: 120_000 });

  if (password) {
    await page.waitForSelector("#filemanager_alert_passwordform_input", {
      timeout: 30_000,
    });
    await page.type("#filemanager_alert_passwordform_input", password);
    await page.click("#filemanager_alert_passwordform_submit");
  }

  await page.waitForFunction(
    () =>
      typeof window.getContent === "function" &&
      (document.querySelector("#filemanager_maincontent[data-item-id]") ||
        document.querySelectorAll("#filemanager_itemslist [data-item-id]").length >
          0),
    { timeout: 120_000 },
  );

  const payload = await page.evaluate(
    async () => {
    const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
    for (let i = 0; i < 120; i += 1) {
      if (typeof window.getContent === "function") {
        break;
      }
      await sleep(500);
    }
    if (typeof window.getContent !== "function") {
      throw new Error("getContent is not available on the share page");
    }

    const main = document.querySelector("#filemanager_maincontent[data-item-id]");
    const pathCode = location.pathname.split("/").filter(Boolean).pop();
    const contentId =
      window.appdata?.fileManager?.mainContent?.data?.id ||
      main?.getAttribute("data-item-id") ||
      pathCode;
    if (!contentId) {
      throw new Error("could not read root content id from the share page");
    }

    async function fetchFolderAll(id) {
      const pageSize = 1000;
      let pageNum = 1;
      let merged = null;

      while (true) {
        const response = await window.getContent(
          id,
          "",
          pageNum,
          pageSize,
          "name",
          1,
        );
        if (!response?.data) {
          throw new Error(`getContent returned no data for ${id}`);
        }
        const chunk = response.data;
        const childCount = Object.keys(chunk.children || {}).length;
        if (!merged) {
          merged = chunk;
        } else {
          Object.assign(merged.children, chunk.children);
        }
        if (childCount < pageSize) {
          break;
        }
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

    const accountActive =
      typeof window.getAccountActive === "function"
        ? await window.getAccountActive()
        : null;

    const root = await fetchFolderAll(contentId);
    const tree = await walkFolder(root);
    return {
      token: accountActive?.token ?? null,
      root: tree,
    };
  },
    { world: "MAIN" },
  );

  process.stdout.write(JSON.stringify(payload));
} catch (error) {
  console.error(error?.stack || String(error));
  process.exit(1);
} finally {
  await browser.close();
}