// Screenshots of the mock-transport app (no daemon needed).
// Usage: npm run build && npm run screenshots [-- <outDir>] [--views eq,conv,...] [--skins midnight,...]
import { preview } from "vite";
import { chromium } from "@playwright/test";
import { mkdirSync } from "node:fs";
import { resolve } from "node:path";

const args = process.argv.slice(2);
const opt = (name, def) => {
  const i = args.indexOf(name);
  return i >= 0 ? args[i + 1] : def;
};
const outDir = resolve(args.find((a, i) => !a.startsWith("--") && !(i > 0 && args[i - 1].startsWith("--"))) ?? "test-results/screenshots");
const skins = opt("--skins", "midnight,graphite,light").split(",");
const views = opt("--views", "eq").split(",");
const sizes = opt("--sizes", "1440x900,1000x700").split(",").map((s) => s.split("x").map(Number));
const query = opt("--query", "");
mkdirSync(outDir, { recursive: true });

const server = await preview({ preview: { port: 1431, strictPort: false }, logLevel: "warn" });
const url = server.resolvedUrls.local[0];
const browser = await chromium.launch();
const errors = [];
try {
  for (const skin of skins) {
    for (const [w, h] of sizes) {
      for (const view of views) {
        const ctx = await browser.newContext({ viewport: { width: w, height: h }, deviceScaleFactor: 1 });
        await ctx.addInitScript(([s, v]) => {
          localStorage.setItem("avdsp-skin", s);
          localStorage.setItem("avdsp-ui", JSON.stringify({ view: v, autoPreamp: true }));
        }, [skin, view]);
        const page = await ctx.newPage();
        page.on("pageerror", (e) => errors.push(`${skin} ${view}: ${e.message}`));
        page.on("console", (m) => m.type() === "error" && errors.push(`${skin} ${view}: ${m.text()}`));
        await page.goto(url + (query ? "?" + query : ""));
        await page.waitForSelector(view === "down" ? ".down-card" : ".app");
        await page.waitForTimeout(900);
        const file = resolve(outDir, `${view}-${skin}-${w}x${h}${query ? "-" + query.replace(/[^a-z0-9]+/gi, "_") : ""}.png`);
        await page.screenshot({ path: file });
        console.log(file);
        await ctx.close();
      }
    }
  }
} finally {
  await browser.close();
  await server.close();
}
if (errors.length) {
  console.error(errors.join("\n"));
  process.exit(1);
}
