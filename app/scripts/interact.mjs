// Interaction smoke test of the mock-transport app (Playwright, Chromium).
// Usage: npm run build && node scripts/interact.mjs <outDir>
import { preview } from "vite";
import { chromium } from "@playwright/test";
import { mkdirSync } from "node:fs";
import { resolve } from "node:path";

const out = resolve(process.argv[2] ?? "test-results/interact");
mkdirSync(out, { recursive: true });
const server = await preview({ preview: { port: 1432, strictPort: false }, logLevel: "warn" });
const url = server.resolvedUrls.local[0];
const browser = await chromium.launch();
const errors = [];
const fail = (m) => {
  errors.push(m);
  console.error("FAIL:", m);
};
async function page(query = "", skin = "midnight", size = [1440, 900]) {
  const ctx = await browser.newContext({ viewport: { width: size[0], height: size[1] } });
  await ctx.addInitScript((s) => localStorage.setItem("avdsp-skin", s), skin);
  const p = await ctx.newPage();
  p.on("pageerror", (e) => fail(`pageerror: ${e.message}`));
  p.on("console", (m) => m.type() === "error" && fail(`console: ${m.text()}`));
  await p.goto(url + query);
  return p;
}
const shot = (p, name) => p.screenshot({ path: resolve(out, name + ".png") });
const bandsOf = (p) => p.evaluate(() => window.avdsp.getState().config.chain.eq.bands);

try {
  // --- EQ interactions
  let p = await page();
  await p.waitForSelector(".app");
  const box = await p.locator(".graph-wrap canvas").boundingBox();
  const hintBox = await p.locator(".graph-hint").boundingBox();
  if (!hintBox) fail("EQ hint missing");
  else if (hintBox.y + hintBox.height > box.y + 0.5) {
    fail(`EQ hint is still inside the plot bounding box: hint bottom ${hintBox.y + hintBox.height} > canvas top ${box.y}`);
  }
  const eqHandle = p.locator(".graph-wrap .resize-handle");
  const ehb = await eqHandle.boundingBox();
  if (!ehb) fail("EQ resize handle missing");
  else {
    await p.mouse.move(ehb.x + ehb.width / 2, ehb.y + ehb.height / 2);
    await p.mouse.down();
    await p.mouse.move(ehb.x + ehb.width / 2, ehb.y + ehb.height / 2 + 200, { steps: 10 });
    await p.mouse.up();
    await p.waitForTimeout(150);
    const grownBox = await p.locator(".graph-wrap canvas").boundingBox();
    if (!(grownBox.height >= box.height + 190)) {
      fail(`dragging EQ handle down 200px did not grow canvas: ${box.height} -> ${grownBox.height}`);
    }
    if (!(await p.evaluate(() => Number(localStorage.getItem("avdsp-height-eq")) > 0))) {
      fail("EQ custom height did not persist to localStorage");
    }
    await eqHandle.dblclick();
    await p.waitForTimeout(150);
    const resetBox = await p.locator(".graph-wrap canvas").boundingBox();
    if (Math.abs(resetBox.height - box.height) > 2) {
      fail(`double-clicking EQ handle did not reset canvas height: ${box.height} -> ${resetBox.height}`);
    }
    if ((await p.evaluate(() => localStorage.getItem("avdsp-height-eq"))) !== null) {
      fail("double-clicking EQ handle did not clear persisted height");
    }
  }
  const before = await bandsOf(p);
  // drag handle 5 (2.55 kHz, +1.6 dB) up and right
  const h = await p.evaluate(() => {
    const cv = document.querySelector(".graph-wrap canvas").getBoundingClientRect();
    const W = cv.width, H = cv.height, f = 2550, g = 1.6;
    const x = 42 + (Math.log10(f / 20) / 3) * (W - 82); // PL 42, PR 40
    const y = 14 + ((15 - g) / 30) * (H - 40);
    return { x: cv.left + x, y: cv.top + y };
  });
  await p.mouse.move(h.x, h.y);
  await p.mouse.down();
  for (let i = 1; i <= 15; i++) await p.mouse.move(h.x + i * 4, h.y - i * 6);
  await p.mouse.up();
  await p.waitForTimeout(300);
  const after = await bandsOf(p);
  if (!(after[4].gain_db > before[4].gain_db + 2 && after[4].freq_hz > before[4].freq_hz)) fail(`drag did not move band 5: ${JSON.stringify(after[4])}`);
  if (!(await p.evaluate(() => window.avdsp.getState().status.preset.modified))) fail("preset not marked modified after drag");
  // wheel changes Q of band 5
  await p.mouse.move(h.x + 60, h.y - 90);
  await p.mouse.wheel(0, -100);
  await p.mouse.wheel(0, -100);
  await p.waitForTimeout(600);
  const q = (await bandsOf(p))[4].q;
  if (!(q > after[4].q)) fail(`wheel did not raise Q (${after[4].q} → ${q})`);
  // double-click empty area adds a band
  await p.mouse.dblclick(box.x + box.width * 0.85, box.y + box.height * 0.8);
  await p.waitForTimeout(300);
  if ((await bandsOf(p)).length !== 8) fail("double-click did not add a band");
  await shot(p, "eq-after-edits");
  // undo x3 (add, wheel, drag)
  for (let i = 0; i < 3; i++) {
    await p.keyboard.press("Control+z");
    await p.waitForTimeout(150);
  }
  const undone = await bandsOf(p);
  if (undone.length !== 7 || undone[4].freq_hz !== before[4].freq_hz) fail(`undo did not restore: ${JSON.stringify(undone[4])}`);
  // editor: type a frequency
  await p.locator(".bchip").nth(2).locator(".sel-btn").click();
  const f = p.getByLabel("Band 3 editor").locator("input").first();
  await f.fill("2.2k");
  await f.press("Enter");
  await p.waitForTimeout(200);
  if ((await bandsOf(p))[2].freq_hz !== 2200) fail("editor frequency entry failed");
  // keyboard on the graph: Delete removes the selected band
  await p.locator(".graph-wrap canvas").focus();
  await p.keyboard.press("Delete");
  await p.waitForTimeout(200);
  if ((await bandsOf(p)).length !== 6) fail("Delete key did not remove the band");
  // overlays: add a generated tilt target from the menu
  if ((await p.evaluate(() => window.avdsp.getState().overlays.length)) < 1) fail("overlays did not load");
  await p.getByRole("button", { name: /Overlays/ }).click();
  await p.waitForSelector(".menu.overlays");
  await p.getByLabel("Built-in target").selectOption("__tilt");
  await p.getByRole("button", { name: "Add", exact: true }).click();
  await p.waitForTimeout(200);
  if (!(await p.evaluate(() => window.avdsp.getState().overlays.some((o) => o.name.startsWith("Tilt"))))) fail("adding a target from the menu failed");
  // compact menu: help lives in the Sources note, opened on demand
  if (await p.locator(".ov-sources").count()) fail("sources note should start closed");
  await p.getByRole("button", { name: "Sources", exact: true }).click();
  const sourcesText = (await p.locator(".ov-sources").textContent()) ?? "";
  for (const needle of ["AutoEq", "B&K 5128", "GRAS KEMAR", "oratory1990", "tilt exploration"]) {
    if (!sourcesText.includes(needle)) fail(`sources note misses ${needle}`);
  }
  if ((await p.locator(".menu.overlays").textContent())?.includes("Owners of Kali")) fail("made-up Kali line is back");
  await shot(p, "eq-overlays-menu");
  await p.keyboard.press("Escape");
  await p.waitForTimeout(150);
  await shot(p, "eq-overlays");

  // fine sliders: arrow 0.5 dB, Shift+arrow 0.1 dB, typed value, balance centre
  const volDb = () => p.evaluate(() => window.avdsp.volume().volume_db);
  const balDb = () => p.evaluate(() => window.avdsp.volume().balance_db);
  const v0 = await volDb();
  await p.locator("#vol").focus();
  await p.keyboard.press("ArrowUp");
  await p.waitForTimeout(120);
  if (Math.abs((await volDb()) - (v0 + 0.5)) > 1e-6) fail(`volume arrow step: ${v0} → ${await volDb()}`);
  await p.keyboard.press("Shift+ArrowUp");
  await p.waitForTimeout(120);
  if (Math.abs((await volDb()) - (v0 + 0.6)) > 1e-6) fail(`volume shift-arrow step: ${await volDb()}`);
  await p.locator("#vol ~ output").dblclick();
  const typed = p.locator(".fine-input");
  await typed.waitFor();
  await typed.fill("-24.8");
  await typed.press("Enter");
  await p.waitForTimeout(150);
  if (Math.abs((await volDb()) + 24.8) > 1e-6) fail(`typed volume: ${await volDb()}`);
  await p.locator("#bal").focus();
  await p.keyboard.press("Shift+ArrowRight");
  await p.waitForTimeout(150);
  if (Math.abs((await balDb()) - 0.1) > 1e-6) fail(`balance step: ${await balDb()}`);
  await p.locator("#bal").dblclick();
  await p.waitForTimeout(150);
  if (Math.abs(await balDb()) > 1e-6) fail(`balance did not centre: ${await balDb()}`);
  // Focus on a slider keeps its keys; give the app shortcuts back.
  await p.locator("#bal").blur();

  // A/B keys
  await p.keyboard.press("b");
  await p.waitForTimeout(150);
  if ((await p.evaluate(() => window.avdsp.getState().status.ab.current)) !== "b") fail("key B did not switch");
  await p.keyboard.press("a");
  // preset menu
  await p.getByRole("button", { name: /^Preset:/ }).click();
  await p.waitForSelector(".menu");
  await shot(p, "preset-menu");
  await p.keyboard.press("Escape");
  // badge popover
  await p.locator(".badge").click();
  await shot(p, "badge-popover");
  await p.keyboard.press("Escape");
  // spectrum controls: toggle Pre/Both/Peak hold and Spectrum on/off
  await p.waitForSelector(".spec-ctls");
  await p.waitForTimeout(200);
  await p.getByRole("button", { name: "Both", exact: true }).click();
  await p.getByRole("button", { name: "Peak hold", exact: true }).click();
  await p.waitForTimeout(150);
  if ((await p.evaluate(() => window.avdsp.getState().spectrum))?.tap !== "both") fail("spectrum tap did not switch to both");
  if (!(await p.evaluate(() => window.avdsp.getState().spectrum.peakHold))) fail("spectrum peakHold did not enable");
  await shot(p, "eq-spectrum-both");
  await p.getByRole("button", { name: "Spectrum", exact: true }).click();
  await p.waitForTimeout(100);
  if (await p.evaluate(() => window.avdsp.getState().spectrum.enabled)) fail("spectrum did not turn off");
  await p.getByRole("button", { name: "Spectrum", exact: true }).click();
  await p.getByRole("button", { name: "Post", exact: true }).click();
  // smoothing selector: 1/12 octave by default, persisted when changed
  if ((await p.evaluate(() => window.avdsp.getState().spectrum.smoothing)) !== 12) fail("spectrum smoothing default is not 1/12");
  await p.getByLabel("Spectrum smoothing").selectOption("3");
  await p.waitForTimeout(100);
  if ((await p.evaluate(() => window.avdsp.getState().spectrum.smoothing)) !== 3) fail("spectrum smoothing did not switch to 1/3");
  await p.getByLabel("Spectrum smoothing").selectOption("12");

  // EQ clip warning + 1-click Auto preamp fix + clip LEDs & headroom
  await p.evaluate(() => window.avdsp.setManualPreamp(12));
  await p.waitForSelector(".clip-warn", { timeout: 2000 });
  await p.waitForSelector(".clip.on", { timeout: 2500 });
  await p.locator(".clip-warn button").click();
  await p.waitForFunction(() => !document.querySelector(".clip-warn"), null, { timeout: 2000 });
  const clipCountBeforeReset = await p.locator(".clip.on").count();
  if (clipCountBeforeReset === 0) fail("expected at least one latched channel clip LED before reset");
  while ((await p.locator(".clip.on").count()) > 0) {
    await p.locator(".clip.on").first().click();
    await p.waitForTimeout(50);
  }
  const headroomText = await p.locator(".meters .headroom").textContent();
  if (!headroomText || !headroomText.startsWith("chain ")) fail(`unexpected chain readout: ${headroomText}`);
  const peakText = await p.locator(".meters .readout").textContent();
  if (!peakText || !/^peak (−∞|[+−]?\d+\.\d dBFS)$/.test(peakText)) fail(`unexpected peak readout: ${peakText}`);
  const statTitles = await p.locator(".stats > span[title]").evaluateAll((els) => els.map((e) => e.getAttribute("title")));
  if (statTitles.length !== 3 || !statTitles[0].includes("time budget") || !statTitles[1].includes("underrun") || !statTitles[2].includes("Output buffer")) {
    fail(`status stats tooltips: ${JSON.stringify(statTitles)}`);
  }

  // Bypass
  await p.getByRole("button", { name: "Bypass" }).click();
  await p.waitForTimeout(300);
  await shot(p, "bypass-on");
  await p.getByRole("button", { name: "Bypass" }).click();
  await p.waitForTimeout(150);

  // Upsampling view: Nyquist zoom + log + impulse plots, source rate picker, off state
  await p.locator(".nav-main", { hasText: "Upsampling" }).click();
  await p.waitForSelector(".ups-plots canvas");
  await p.waitForFunction(() => document.querySelectorAll(".ups-plots canvas").length === 3, null, { timeout: 3000 });
  // cutoff tooltips carry real numbers
  const cutTips = await p.getByRole("group", { name: "Cutoff" }).getByRole("button").evaluateAll((els) => els.map((e) => e.getAttribute("title") ?? ""));
  if (cutTips.length !== 3 || !cutTips[0].includes("22.05 kHz") || !cutTips[1].includes("Stop at Nyquist") || !cutTips[2].includes("20.95 kHz") || !cutTips[2].includes("no imaging")) fail(`cutoff tooltips: ${JSON.stringify(cutTips)}`);
  // options the daemon would reject are disabled with the reason as tooltip
  await p.evaluate(() => window.avdsp.setUpsampler({ taps: 64, kaiser_beta: 30 }));
  await p.waitForFunction(() => window.avdsp.getState().config.chain.upsampler.taps === 64, null, { timeout: 2000 }).catch(() => fail("could not set 64 taps / beta 30"));
  await p.waitForTimeout(150);
  const blocked = await p.evaluate(() => {
    const btn = (group, text) => [...document.querySelectorAll(`[role=group][aria-label="${group}"] button`)].find((b) => b.textContent === text);
    const ns = btn("Cutoff", "Stop at Nyquist");
    const mp = btn("Phase", "Minimum");
    return { ns: [ns?.disabled, ns?.title], mp: [mp?.disabled, mp?.title] };
  });
  if (!blocked.ns[0] || !blocked.ns[1]?.includes("between 5 and 24")) fail(`Stop at Nyquist not blocked for beta 30: ${JSON.stringify(blocked)}`);
  if (!blocked.mp[0] || !blocked.mp[1]?.includes("between 2 and 20")) fail(`Minimum phase not blocked for beta 30: ${JSON.stringify(blocked)}`);
  await p.evaluate(() => window.avdsp.setUpsampler({ kaiser_beta: 14 }));
  await p.waitForTimeout(150);
  const shortTip = await p.evaluate(() => {
    const ns = [...document.querySelectorAll('[role=group][aria-label="Cutoff"] button')].find((b) => b.textContent === "Stop at Nyquist");
    return [ns?.disabled, ns?.title];
  });
  if (!shortTip[0] || !shortTip[1]?.includes("only guaranteed from")) fail(`Stop at Nyquist not blocked for 64 taps at x4: ${JSON.stringify(shortTip)}`);
  await p.evaluate(() => window.avdsp.setUpsampler({ taps: 65536 }));
  await p.waitForFunction(() => window.avdsp.getState().config.chain.upsampler.taps === 65536, null, { timeout: 2000 }).catch(() => fail("could not restore 64k taps"));
  await p.waitForSelector(".ups-plots canvas");
  // zoom: wheel, box, pan, crossings, double-click reset, precise window refetch
  const trans = p.locator(".ups-plots canvas").first();
  await trans.waitFor();
  const tb = await trans.boundingBox();
  await p.waitForSelector(".crossing");
  const nCross = await p.locator(".crossing").count();
  if (nCross < 8) fail(`expected the threshold crossings, got ${nCross}`);
  await p.mouse.move(tb.x + tb.width * 0.45, tb.y + tb.height * 0.5);
  for (let i = 0; i < 6; i++) await p.mouse.wheel(0, -240);
  await p.waitForTimeout(150);
  await p.waitForSelector(".plot-tools button:not([disabled])", { timeout: 2000 }).catch(() => fail("Reset did not enable after wheel zoom"));
  // box zoom (x only by default)
  await p.mouse.move(tb.x + tb.width * 0.4, tb.y + tb.height * 0.4);
  await p.mouse.down();
  await p.mouse.move(tb.x + tb.width * 0.6, tb.y + tb.height * 0.6, { steps: 4 });
  await p.mouse.up();
  // pan with Ctrl-drag
  await p.keyboard.down("Control");
  await p.mouse.move(tb.x + tb.width * 0.5, tb.y + tb.height * 0.5);
  await p.mouse.down();
  await p.mouse.move(tb.x + tb.width * 0.55, tb.y + tb.height * 0.5, { steps: 3 });
  await p.mouse.up();
  await p.keyboard.up("Control");
  // crossings zoom the transition plot; hover shows a precise readout
  await p.locator(".crossing").nth(1).click();
  await p.waitForTimeout(500);
  if ((await p.locator('.crossing[aria-pressed="true"]').count()) !== 1) fail("crossing did not select");
  await p.mouse.move(tb.x + tb.width * 0.5, tb.y + tb.height * 0.5);
  await p.waitForTimeout(400);
  await shot(p, "upsampling-zoomed");
  await trans.dblclick({ position: { x: tb.width * 0.5, y: tb.height * 0.5 } });
  await p.waitForTimeout(200);
  if (await p.locator(".plot-tools button:not([disabled])", { hasText: "Reset" }).count()) {
    // the full-range plot is not zoomed and the transition was reset by double-click
    fail("double-click did not reset the transition plot");
  }
  // stats cards show precise values without truncation
  const facts = await p.locator(".fact .v").evaluateAll((els) => els.map((e) => [e.textContent, e.scrollWidth <= e.clientWidth + 1]));
  if (facts.some(([, fits]) => !fits)) fail(`a stats card overflows: ${JSON.stringify(facts)}`);
  if (!facts.some(([t]) => /\d,\d{3}\.\d{3} Hz/.test(t ?? ""))) fail(`stats lack precise Hz values: ${JSON.stringify(facts)}`);
  await p.getByRole("group", { name: "Source rate" }).getByRole("button", { name: "96k" }).click();
  await p.waitForFunction(() => document.body.innerText.includes("96 kHz → 384 kHz"), null, { timeout: 3000 });
  await shot(p, "upsampling-96k");
  await p.getByRole("group", { name: "Ratio" }).getByRole("button", { name: "Off" }).click();
  await p.waitForFunction(() => document.body.innerText.includes("1:1 bypass"), null, { timeout: 3000 });
  await p.getByRole("group", { name: "Ratio" }).getByRole("button", { name: "×4" }).click();
  await p.waitForFunction(() => document.body.innerText.includes("96 kHz → 384 kHz"), null, { timeout: 3000 });
  // Advanced disclosure: starts collapsed, opens, remembers state, Deeper sets beta=17 with tooltip
  if ((await p.getByRole("group", { name: "Filter shape (Kaiser β)" }).count()) !== 0) fail("Advanced should start collapsed");
  await p.getByRole("button", { name: "Advanced" }).click();
  const betaGroup = p.getByRole("group", { name: "Filter shape (Kaiser β)" });
  await betaGroup.waitFor();
  if ((await p.evaluate(() => localStorage.getItem("avdsp-upsampling-advanced"))) !== "1") fail("Advanced state did not persist to localStorage");
  const betaTips = await betaGroup.getByRole("button").evaluateAll((els) => els.map((e) => [e.textContent, e.getAttribute("title") ?? ""]));
  if (betaTips.length !== 4 || betaTips.some(([, title]) => !title || !title.includes("dB") || !title.includes("transition") || !title.includes("24-bit"))) {
    fail(`Kaiser beta tooltips missing expected text: ${JSON.stringify(betaTips)}`);
  }
  const deeperBtn = betaGroup.getByRole("button", { name: "Deeper" });
  await deeperBtn.click();
  await p.waitForFunction(() => window.avdsp.getState().config.chain.upsampler.kaiser_beta === 17, null, { timeout: 2000 }).catch(() =>
    fail("clicking Deeper did not set kaiser_beta to 17"),
  );
  if ((await deeperBtn.getAttribute("aria-pressed")) !== "true") fail("Deeper button not marked pressed");
  const deeperTitle = (await deeperBtn.getAttribute("title")) ?? "";
  if (!deeperTitle.includes("163 dB") || !deeperTitle.includes("transition")) fail(`unexpected Deeper tooltip: ${deeperTitle}`);
  await shot(p, "upsampling-advanced");
  // Upsampling resize handles: Nyquist transition + shared Full-range/Impulse row
  const transBefore = await p.locator(".ups-plots canvas").nth(0).boundingBox();
  const fullBefore = await p.locator(".ups-plots canvas").nth(1).boundingBox();
  const impBefore = await p.locator(".ups-plots canvas").nth(2).boundingBox();
  const transHandle = p.getByRole("separator", { name: "Resize Nyquist transition plot height" });
  await transHandle.focus();
  await p.keyboard.press("Shift+ArrowDown");
  await p.waitForTimeout(100);
  const transGrown = await p.locator(".ups-plots canvas").nth(0).boundingBox();
  if (Math.abs(transGrown.height - (transBefore.height + 100)) > 2) {
    fail(`Shift+ArrowDown on Nyquist transition handle did not grow by 100px: ${transBefore.height} -> ${transGrown.height}`);
  }
  await transHandle.dblclick();
  await p.waitForTimeout(100);
  const transReset = await p.locator(".ups-plots canvas").nth(0).boundingBox();
  if (Math.abs(transReset.height - transBefore.height) > 2) {
    fail(`double-click on Nyquist transition handle did not reset: ${transBefore.height} -> ${transReset.height}`);
  }
  const subHandle = p.getByRole("separator", { name: "Resize Full-range and Impulse plots height" });
  await subHandle.focus();
  await p.keyboard.press("Shift+ArrowDown");
  await p.waitForTimeout(100);
  const fullGrown = await p.locator(".ups-plots canvas").nth(1).boundingBox();
  const impGrown = await p.locator(".ups-plots canvas").nth(2).boundingBox();
  if (Math.abs(fullGrown.height - (fullBefore.height + 100)) > 2 || Math.abs(impGrown.height - (impBefore.height + 100)) > 2) {
    fail(`shared sub-plots handle did not grow both canvases together: full ${fullBefore.height}->${fullGrown.height}, imp ${impBefore.height}->${impGrown.height}`);
  }
  await subHandle.dblclick();
  await p.waitForTimeout(100);
  const fullReset = await p.locator(".ups-plots canvas").nth(1).boundingBox();
  const impReset = await p.locator(".ups-plots canvas").nth(2).boundingBox();
  if (Math.abs(fullReset.height - fullBefore.height) > 2 || Math.abs(impReset.height - impBefore.height) > 2) {
    fail(`double-click on shared sub-plots handle did not reset both canvases`);
  }
  await p.close();

  // --- digital volume DAC: DIG badge, and bit-perfect (Bypass) disables the slider
  p = await page("?digital");
  await p.waitForSelector(".app");
  const dig = p.locator(".ctl .sw");
  if ((await dig.textContent()) !== "DIG") fail("DIG badge missing for a DAC without hardware volume");
  if (!((await dig.getAttribute("title")) ?? "").includes("64-bit float")) fail("DIG badge tooltip does not explain digital volume");
  if ((await p.locator("#vol").getAttribute("aria-disabled")) === "true") fail("volume should be enabled on the DSP path");
  await p.getByRole("button", { name: "Bypass" }).click();
  await p.waitForSelector(".ctl.bit-perfect", { timeout: 2000 }).catch(() => fail("bit-perfect state did not appear on Bypass"));
  if ((await p.locator("#vol ~ output").textContent()) !== "on DAC") fail("volume readout should say the volume is on the DAC");
  await p
    .waitForFunction(() => document.querySelector(".meters .headroom")?.textContent === "chain —", null, { timeout: 2000 })
    .catch(() => fail("chain gain should be blank while the chain is bypassed"));
  if ((await p.locator("#vol").getAttribute("aria-disabled")) !== "true") fail("volume slider should be disabled while bit-perfect");
  if (!((await p.locator(".ctl.bit-perfect").getAttribute("title")) ?? "").includes("set the volume on the DAC")) fail("bit-perfect hint missing");
  await shot(p, "topbar-bit-perfect");
  await p.getByRole("button", { name: "Bypass" }).click();
  await p.close();

  // --- split L/R
  p = await page("?split");
  await p.waitForSelector(".app");
  await p.waitForTimeout(500);
  const splitCanvasBox = await p.locator(".graph-wrap canvas").boundingBox();
  const legendBox = await p.locator(".legend").boundingBox();
  if (!legendBox) fail("split EQ legend missing");
  else if (legendBox.y + legendBox.height > splitCanvasBox.y + 0.5) {
    fail(`EQ legend is still inside the plot bounding box: legend bottom ${legendBox.y + legendBox.height} > canvas top ${splitCanvasBox.y}`);
  }
  await shot(p, "eq-split");
  await p.close();

  // --- engine down, then start
  p = await page("?engine=down", "graphite", [1000, 700]);
  await p.waitForSelector(".down-card");
  await shot(p, "engine-down");
  await p.getByRole("button", { name: "Start engine" }).click();
  await p.waitForSelector(".app", { timeout: 5000 });
  await p.close();

  // --- device self-test, light theme, narrow
  p = await page("", "light", [1000, 700]);
  await p.waitForSelector(".app");
  await p.getByRole("button", { name: /^Device/ }).click();
  await p.getByRole("button", { name: "Run bit-perfect self-test" }).click();
  await p.waitForSelector(".result:not(.pending)", { timeout: 5000 });
  await shot(p, "device-selftest");
  await p.close();
} finally {
  await browser.close();
  await server.close();
}
if (errors.length) process.exit(1);
console.log("interaction checks passed");
