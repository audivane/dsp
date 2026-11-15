// Cropped screenshots of the mock-transport app for the audivane.app website.
// Usage: npm run build && npm run screenshots:showcase -- <outDir> [--skins light,midnight]
//
// Every shot runs the real UI against the in-memory mock daemon (no engine,
// no audio): a split L/R EQ with no overlays, the live (simulated) spectrum,
// meters and the measured upsampler view. Output: PNG files named
// `<shot>-<skin>.png`, captured at 2x device scale. The website converts and
// commits them; it never ships this app's JavaScript.
import { preview } from "vite";
import { chromium } from "@playwright/test";
import { mkdirSync } from "node:fs";
import { resolve } from "node:path";

const args = process.argv.slice(2);
const opt = (name, def) => {
  const i = args.indexOf(name);
  return i >= 0 ? args[i + 1] : def;
};
const outDir = resolve(args.find((a, i) => !a.startsWith("--") && !(i > 0 && args[i - 1].startsWith("--"))) ?? "test-results/showcase");
const skins = opt("--skins", "light,midnight").split(",");
const scale = Number(opt("--scale", "2"));
mkdirSync(outDir, { recursive: true });

const QUERY = "split&overlays=none";
const SHOTS = [
  // The EQ plot alone (axes, curves, spectrum; no hint or legend row): front-page visual.
  { name: "eq-graph", view: "eq", viewport: [1240, 780], clip: ".eq-view .graph-stage", closeEditor: true },
  // The status bar: rate path, DSP load, L/R peak meters and clip indicators.
  { name: "status", view: "eq", viewport: [1180, 760], clip: "footer.status", closeEditor: true },
  // Measured upsampler response: transition, full range and impulse.
  { name: "upsampling", view: "ups", viewport: [1440, 1100], clip: ".ups-plots" },
  // The full window: sidebar signal path, spectrum behind the EQ, status bar meters.
  { name: "app", view: "eq", viewport: [1440, 900], clip: null, closeEditor: true },
];

const server = await preview({ preview: { port: 1432, strictPort: false }, logLevel: "warn" });
const url = server.resolvedUrls.local[0];
const browser = await chromium.launch();
const errors = [];
try {
  for (const skin of skins) {
    for (const shot of SHOTS) {
      const [w, h] = shot.viewport;
      const ctx = await browser.newContext({ viewport: { width: w, height: h }, deviceScaleFactor: scale });
      await ctx.addInitScript(([s, v]) => {
        localStorage.setItem("avdsp-skin", s);
        localStorage.setItem("avdsp-ui", JSON.stringify({ view: v, autoPreamp: true }));
      }, [skin, shot.view]);
      const page = await ctx.newPage();
      page.on("pageerror", (e) => errors.push(`${skin} ${shot.name}: ${e.message}`));
      page.on("console", (m) => m.type() === "error" && errors.push(`${skin} ${shot.name}: ${m.text()}`));
      await page.goto(`${url}?${QUERY}`);
      await page.waitForSelector(".app");
      if (shot.closeEditor) {
        const close = page.getByRole("button", { name: "Close editor" });
        if (await close.count()) await close.click();
      }
      if (shot.clip) await page.waitForSelector(shot.clip);
      // Let the simulated meters and spectrum settle.
      await page.waitForTimeout(1500);
      const file = resolve(outDir, `${shot.name}-${skin}.png`);
      if (shot.clip) await page.locator(shot.clip).first().screenshot({ path: file, animations: "disabled" });
      else await page.screenshot({ path: file, animations: "disabled" });
      console.log(file);
      await ctx.close();
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
