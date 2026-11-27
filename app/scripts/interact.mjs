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
  {
    const h = await p.locator(".status").evaluate((el) => {
      const kids = [...el.children].filter((c) => getComputedStyle(c).display !== "none");
      return new Set(kids.map((c) => Math.round(c.getBoundingClientRect().top / 10))).size;
    });
    if (h !== 1) fail(`status bar wraps at 1440 px (${h} rows)`);
  }
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
  // a double-click on the editor's chrome, then press and drag: no page-wide
  // text selection (the fields stay selectable)
  {
    const ed = p.getByLabel("Band 3 editor");
    const seg = await ed.locator(".types").boundingBox();
    const edb = await ed.boundingBox();
    const selLen = () => p.evaluate(() => window.getSelection()?.toString().length ?? 0);
    for (const [x, y] of [[seg.x + 1, seg.y + 1], [edb.x + edb.width - 6, edb.y + edb.height - 4]]) {
      await p.mouse.move(x, y);
      await p.mouse.down({ clickCount: 1 });
      await p.mouse.up({ clickCount: 1 });
      await p.mouse.down({ clickCount: 2 });
      await p.mouse.up({ clickCount: 2 });
      await p.mouse.down({ clickCount: 3 });
      await p.mouse.move(box.x + 80, box.y + box.height - 10, { steps: 8 });
      await p.mouse.move(box.x + 10, 30, { steps: 8 });
      const n = await selLen();
      await p.mouse.up();
      if (n > 0) fail(`dragging after a double-click on the band editor selected ${n} characters of page text`);
      await p.evaluate(() => window.getSelection()?.removeAllRanges());
    }
    await f.dblclick();
    const picked = await f.evaluate((el) => el.selectionEnd - el.selectionStart);
    if (!(picked > 0)) fail("double-click in an editor field no longer selects its text");
    await f.press("Escape");
  }
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

  // A press on the knob begins the gesture from the level shown: when the
  // level changes (the desktop) before the first move, the drag is refused.
  const knob = await p.locator("#vol .fine-knob").boundingBox();
  const kx = knob.x + knob.width / 2;
  const ky = knob.y + knob.height / 2;
  await p.mouse.move(kx, ky);
  await p.mouse.down();
  await p.evaluate(() => window.avdsp.transport.daemon.setLevel({ volume_db: -40 }));
  await p.waitForTimeout(150);
  await p.mouse.move(kx + 4, ky);
  await p.mouse.up();
  await p.waitForTimeout(400);
  const daemonDb = await p.evaluate(() => window.avdsp.transport.daemon.volume.volume_db);
  if (daemonDb !== -40) fail(`a drag begun before an outside level change was applied: ${daemonDb}`);
  if ((await volDb()) !== -40) fail(`the app does not show the outside level after the refused drag: ${await volDb()}`);
  await p.locator("#vol").blur();

  const daemonVol = () => p.evaluate(() => window.avdsp.transport.daemon.volume);
  const knobAt = async () => {
    const b = await p.locator("#vol .fine-knob").boundingBox();
    return [b.x + b.width / 2, b.y + b.height / 2];
  };
  // A mute clicked during a wheel burst stays: the burst's close does not
  // rewrite the mute.
  await p.evaluate(() => window.avdsp.transport.daemon.setLevel({ volume_db: -12 }));
  await p.waitForFunction(() => window.avdsp.volume()?.volume_db === -12);
  {
    const [x, y] = await knobAt();
    await p.mouse.move(x, y);
    await p.mouse.wheel(0, 100);
    await p.waitForTimeout(80);
    await p.locator("button.mute").click();
    await p.waitForTimeout(700);
    const d = await daemonVol();
    if (!d.muted || d.volume_db !== -12.5) fail(`a wheel burst undid a mute: ${JSON.stringify(d)}`);
    if (!(await p.evaluate(() => window.avdsp.volume()?.muted))) fail("the app shows unmuted after a mute during a wheel burst");
    await p.locator("button.mute").click();
    await p.waitForTimeout(200);
  }
  // A refused pointer drag stays dead while the wheel works on the same
  // slider: its later moves and release write nothing.
  await p.evaluate(() => window.avdsp.transport.daemon.setLevel({ volume_db: -12, muted: false }));
  await p.waitForFunction(() => window.avdsp.volume()?.volume_db === -12);
  {
    const [x, y] = await knobAt();
    const box = await p.locator("#vol").boundingBox();
    await p.mouse.move(x, y);
    await p.mouse.down();
    await p.evaluate(() => window.avdsp.transport.daemon.setLevel({ volume_db: -40 }));
    await p.waitForTimeout(150);
    await p.mouse.move(x + 3, y);
    await p.waitForTimeout(250);
    await p.mouse.wheel(0, 100);
    await p.waitForTimeout(150);
    await p.mouse.move(Math.min(x + 40, box.x + box.width - 2), y);
    await p.mouse.up();
    await p.waitForTimeout(600);
    const d = await daemonVol();
    if (d.volume_db !== -40.5) fail(`a refused drag was revived by a wheel step: ${d.volume_db}`);
  }
  // A drag's final step goes out at its release, not behind a DSP toggle
  // clicked right after (here a slow one).
  await p.evaluate(() => window.avdsp.transport.daemon.setLevel({ volume_db: -12, muted: false }));
  await p.waitForFunction(() => window.avdsp.volume()?.volume_db === -12);
  await p.evaluate(() => {
    const t = window.avdsp.transport;
    t.slowToggle = t.call;
    t.call = async (m, params) => {
      if (m === "set_dsp_enabled") await new Promise((r) => setTimeout(r, 600));
      return t.slowToggle(m, params);
    };
  });
  {
    const [x, y] = await knobAt();
    await p.mouse.move(x, y);
    await p.mouse.down();
    await p.mouse.move(x - 6, y);
    await p.mouse.move(x - 12, y);
    await p.mouse.up();
    // What the Bypass button does, within the throttle's 40 ms.
    await p.evaluate(() => window.avdsp.setBypass(true));
    await p.waitForTimeout(250);
    const shown = await volDb();
    const d = await daemonVol();
    if (d.volume_db !== shown) fail(`a released drag's final step waited behind a DSP toggle: daemon ${d.volume_db}, shown ${shown}`);
    await p.waitForTimeout(600);
    await p.evaluate(() => {
      const t = window.avdsp.transport;
      t.call = t.slowToggle;
      delete t.slowToggle;
    });
    await p.getByRole("button", { name: "Bypass" }).click();
    await p.waitForTimeout(200);
  }
  await p.locator("#vol").blur();

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
  await p.locator(".spec-ctls").getByRole("button", { name: "Both", exact: true }).click();
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

  // Upsampling view: Nyquist zoom + impulse plots, source rate picker, off state
  await p.locator(".nav-main", { hasText: "Upsampling" }).click();
  await p.waitForSelector(".ups-plots canvas");
  await p.waitForFunction(() => document.querySelectorAll(".ups-plots canvas").length === 2, null, { timeout: 3000 }).catch(() => fail("the Upsampling page should have two plots (Nyquist transition, impulse)"));
  if (await p.locator(".ups-plot-card", { hasText: "Full-range response" }).count()) fail("the Full-range response plot is still there");
  // Custom v2: the design editor shows the engine's plan per rate, and an
  // edit that does not pass stays a draft with the error next to its field.
  const DESIGN = { kind: "windowed_sinc", window: { type: "kaiser", beta: 14, sampling: "symmetric" }, length: { rule: "fixed", taps: 65536 }, decimation: { length: "keep" }, cutoff: { mode: "fraction", value: 1 }, phase: { type: "linear" }, preserve_samples: false, rates: { accept: "any", fallback_window: null }, numerics: { path: "standard", grid_cutoff: "design_ratio" } };
  const design = () => p.evaluate(() => window.avdsp.getState().config.chain.upsampler.design);
  await p.waitForSelector(".ups-plan-table tbody tr", { timeout: 3000 }).catch(() => fail("no per-rate plan in the Custom view"));
  const planRows = await p.locator(".ups-plan-table tbody tr").count();
  if (planRows < 2) fail(`plan rows: ${planRows}`);
  if (!((await p.locator(".ups-plan").textContent()) ?? "").includes("Latency at 44.1 kHz")) fail("no latency readout in the plan");
  {
    // The plan sits beside the design rows on a wide window, below them on a narrow one.
    const layout = () =>
      p.evaluate(() => {
        const plan = document.querySelector(".ups-plan").getBoundingClientRect();
        const rows = document.querySelector(".ups-design-rows").getBoundingClientRect();
        return { beside: plan.left >= rows.right && plan.top < rows.bottom, below: plan.top >= rows.bottom };
      });
    if (!(await layout()).beside) fail(`the plan is not beside the design rows at 1440 px: ${JSON.stringify(await layout())}`);
    await p.setViewportSize({ width: 1000, height: 900 });
    await p.waitForFunction(() => document.querySelector(".ups-plan").getBoundingClientRect().top >= document.querySelector(".ups-design-rows").getBoundingClientRect().bottom, null, { timeout: 2000 }).catch(async () => fail(`the plan is not below the design rows at 1000 px: ${JSON.stringify(await layout())}`));
    await p.setViewportSize({ width: 1440, height: 900 });
  }
  {
    const beta = p.locator(".field", { has: p.locator(".lbl", { hasText: /^Beta$/ }) }).locator("input");
    await beta.fill("30");
    await beta.press("Enter");
    await p.waitForFunction(() => window.avdsp.getState().config.chain.upsampler.design?.window?.beta === 30, null, { timeout: 2000 }).catch(() => fail("could not set beta 30"));
    // Minimum phase by zero reflection takes beta 2..20: not sent, the error shows at the window.
    await p.getByRole("group", { name: "Phase", exact: true }).getByRole("button", { name: "Minimum" }).click();
    await p.waitForSelector(".ups-field-err", { timeout: 3000 }).catch(() => fail("no field error for beta 30 with minimum phase"));
    const errText = (await p.locator(".ups-field-err").first().textContent()) ?? "";
    if (!errText.includes("between 2 and 20")) fail(`field error text: ${errText}`);
    if ((await design()).phase.type !== "linear") fail("an edit that does not pass was sent");
    await p.waitForSelector(".ups-draft", { timeout: 2000 }).catch(() => fail("no Not applied banner for a failing edit"));
    await shot(p, "upsampling-custom-error");
    // Fixing the field sends the edit.
    await beta.fill("14");
    await beta.press("Enter");
    await p.waitForFunction(() => { const d = window.avdsp.getState().config.chain.upsampler.design; return d?.phase?.type === "minimum" && d.window.beta === 14; }, null, { timeout: 3000 }).catch(async () => fail(`the fixed edit was not sent: ${JSON.stringify(await design())}`));
    if (await p.locator(".ups-draft").count()) fail("the draft banner stays after the fix");
    await p.evaluate((d) => window.avdsp.setUpsampler({ design: d }), DESIGN);
    await p.waitForFunction(() => window.avdsp.getState().config.chain.upsampler.design?.phase?.type === "linear", null, { timeout: 2000 });
  }
  {
    // The taps field takes 2^20, 64k and 1M; the menu offers powers of two.
    const taps = p.locator(".field", { has: p.locator(".lbl", { hasText: /^Taps$/ }) }).locator("input");
    const tapsNow = () => p.evaluate(() => window.avdsp.getState().config.chain.upsampler.design?.length?.taps);
    for (const [text, want] of [["2^17", 131072], ["256k", 262144], ["64K", 65536]]) {
      await taps.fill(text);
      await taps.press("Enter");
      await p.waitForFunction((n) => window.avdsp.getState().config.chain.upsampler.design?.length?.taps === n, want, { timeout: 2000 }).catch(async () => fail(`taps "${text}": ${await tapsNow()}`));
    }
    await p.getByRole("combobox", { name: "Common tap counts" }).selectOption("131072");
    await p.waitForFunction(() => window.avdsp.getState().config.chain.upsampler.design?.length?.taps === 131072, null, { timeout: 2000 }).catch(async () => fail(`taps menu: ${await tapsNow()}`));
    // β: given with a fraction cutoff, derived with a band cutoff (no field, no toggle).
    if (await p.getByRole("button", { name: /^(Derive|Set β)$/ }).count()) fail("the β Derive / Set toggle is still there");
    const cutoffSeg = p.getByRole("group", { name: "Cutoff mode" });
    await cutoffSeg.getByRole("button", { name: "Band" }).click();
    await p.waitForFunction(() => { const d = window.avdsp.getState().config.chain.upsampler.design; return d?.cutoff?.mode === "band" && d.window.beta === null; }, null, { timeout: 3000 }).catch(async () => fail(`Band did not derive β: ${JSON.stringify(await design())}`));
    if (await p.locator(".field", { has: p.locator(".lbl", { hasText: /^Beta$/ }) }).count()) fail("a β field shows with the Band cutoff");
    if (!((await p.locator(".ups-derived").textContent()) ?? "").includes("from the band depth")) fail(`derived β text: ${await p.locator(".ups-derived").textContent()}`);
    await cutoffSeg.getByRole("button", { name: "Stop at Nyquist" }).click();
    await p.waitForFunction(() => { const d = window.avdsp.getState().config.chain.upsampler.design; return d?.cutoff?.mode === "nyquist_stop" && d.window.beta === 14; }, null, { timeout: 3000 }).catch(async () => fail(`Stop at Nyquist did not give β: ${JSON.stringify(await design())}`));
    if (await p.locator(".ups-field-err").count()) fail(`field error after the cutoff switches: ${await p.locator(".ups-field-err").first().textContent()}`);
    await p.evaluate((d) => window.avdsp.setUpsampler({ design: d }), DESIGN);
    await p.waitForFunction(() => window.avdsp.getState().config.chain.upsampler.design?.cutoff?.mode === "fraction", null, { timeout: 2000 });
  }
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
  // The impulse plot zooms (wheel, box), shows a sample readout and resets on double-click.
  {
    const imp = p.locator(".ups-plots canvas").nth(1);
    const ib = await imp.boundingBox();
    const label = () => imp.getAttribute("aria-label");
    const home = await label();
    if (ib.width < tb.width - 4) fail(`the impulse plot is not full width: ${ib.width} vs ${tb.width}`);
    await p.mouse.move(ib.x + ib.width * 0.5, ib.y + ib.height * 0.5);
    for (let i = 0; i < 6; i++) await p.mouse.wheel(0, -240);
    await p.waitForFunction((h) => document.querySelectorAll(".ups-plots canvas")[1].getAttribute("aria-label") !== h, home, { timeout: 2000 }).catch(() => fail("wheel did not zoom the impulse plot"));
    if (!(await p.locator(".plot-tools button:not([disabled])", { hasText: "Reset" }).count())) fail("Reset did not enable after zooming the impulse plot");
    const wheeled = await label();
    await p.mouse.move(ib.x + ib.width * 0.45, ib.y + ib.height * 0.5);
    await p.mouse.down();
    await p.mouse.move(ib.x + ib.width * 0.55, ib.y + ib.height * 0.5, { steps: 4 });
    await p.mouse.up();
    await p.waitForFunction((h) => document.querySelectorAll(".ups-plots canvas")[1].getAttribute("aria-label") !== h, wheeled, { timeout: 2000 }).catch(() => fail("box zoom did not zoom the impulse plot"));
    await p.mouse.move(ib.x + ib.width * 0.5, ib.y + ib.height * 0.5);
    await p.waitForTimeout(200);
    await shot(p, "upsampling-impulse-zoomed");
    await imp.dblclick({ position: { x: ib.width * 0.5, y: ib.height * 0.5 } });
    await p.waitForFunction((h) => document.querySelectorAll(".ups-plots canvas")[1].getAttribute("aria-label") === h, home, { timeout: 2000 }).catch(() => fail("double-click did not reset the impulse plot"));
    if (await p.locator(".plot-tools button:not([disabled])", { hasText: "Reset" }).count()) fail("Reset stays enabled after both plots reset");
  }
  // stats cards fit, without the facts the plots already show
  const facts = await p.locator(".fact .v").evaluateAll((els) => els.map((e) => [e.textContent, e.scrollWidth <= e.clientWidth + 1]));
  if (facts.some(([, fits]) => !fits)) fail(`a stats card overflows: ${JSON.stringify(facts)}`);
  if (!facts.some(([t]) => /\d,\d{3}\.\d Hz/.test(t ?? ""))) fail(`stats lack the stopband start: ${JSON.stringify(facts)}`);
  const factKeys = await p.locator(".facts .fact .k").allTextContents();
  if (factKeys.some((k) => /dB at|Taps/.test(k))) fail(`removed facts still shown: ${JSON.stringify(factKeys)}`);
  await p.getByRole("group", { name: "Preview at source rate" }).getByRole("button", { name: "96k" }).click();
  await p.waitForFunction(() => document.body.innerText.includes("96 kHz → 384 kHz"), null, { timeout: 3000 });
  await shot(p, "upsampling-96k");
  await p.getByRole("group", { name: "Rate", exact: true }).getByRole("button", { name: "Off" }).click();
  // Off: the filter stays editable and the plots preview it.
  await p.waitForFunction(() => window.avdsp.getState().config.chain.upsampler.target === "off" && !!document.querySelector(".ups-plots") && !document.querySelector('[role=group][aria-label="Filter design"] button[disabled]'), null, { timeout: 3000 }).catch(() => fail("Off hides or disables the filter"));
  // Multiply returns to the last ratio used (×4).
  await p.getByRole("group", { name: "Rate", exact: true }).getByRole("button", { name: "Multiply" }).click();
  await p.waitForFunction(() => JSON.stringify(window.avdsp.getState().config.chain.upsampler.target) === '{"ratio":4}', null, { timeout: 2000 }).catch(() => fail("Multiply did not return to x4"));
  await p.waitForFunction(() => document.body.innerText.includes("96 kHz → 384 kHz"), null, { timeout: 3000 });
  // Advanced disclosure: starts collapsed, opens, remembers state; it holds the arithmetic.
  if ((await p.getByRole("combobox", { name: "Arithmetic path" }).count()) !== 0) fail("Advanced should start collapsed");
  await p.getByRole("button", { name: "Advanced" }).click();
  await p.getByRole("combobox", { name: "Arithmetic path" }).waitFor({ timeout: 2000 }).catch(() => fail("Advanced has no arithmetic path"));
  if ((await p.evaluate(() => localStorage.getItem("avdsp-upsampling-advanced"))) !== "1") fail("Advanced state did not persist to localStorage");
  // A window the standard path does not build moves the design to the FIR path.
  await p.getByRole("group", { name: "Window", exact: true }).getByRole("button", { name: "Flat-top" }).click();
  await p.waitForFunction(() => { const d = window.avdsp.getState().config.chain.upsampler.design; return d?.window?.type === "flattop" && d.numerics.path === "fir"; }, null, { timeout: 3000 }).catch(async () => fail(`Flat-top not applied on the FIR path: ${JSON.stringify(await design())}`));
  if ((await p.getByRole("combobox", { name: "Arithmetic path" }).inputValue()) !== "fir") fail("the arithmetic path does not show FIR");
  await shot(p, "upsampling-advanced");
  await p.evaluate((d) => window.avdsp.setUpsampler({ design: d }), DESIGN);
  await p.waitForFunction(() => window.avdsp.getState().config.chain.upsampler.design?.window?.type === "kaiser", null, { timeout: 2000 });
  // Upsampling resize handles: Nyquist transition and Impulse response
  const transBefore = await p.locator(".ups-plots canvas").nth(0).boundingBox();
  const impBefore = await p.locator(".ups-plots canvas").nth(1).boundingBox();
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
  const impHandle = p.getByRole("separator", { name: "Resize Impulse response plot height" });
  await impHandle.focus();
  await p.keyboard.press("Shift+ArrowDown");
  await p.waitForTimeout(100);
  const impGrown = await p.locator(".ups-plots canvas").nth(1).boundingBox();
  if (Math.abs(impGrown.height - (impBefore.height + 100)) > 2) fail(`the impulse handle did not grow the plot by 100px: ${impBefore.height}->${impGrown.height}`);
  await impHandle.dblclick();
  await p.waitForTimeout(100);
  const impReset = await p.locator(".ups-plots canvas").nth(1).boundingBox();
  if (Math.abs(impReset.height - impBefore.height) > 2) fail(`double-click on the impulse handle did not reset: ${impBefore.height}->${impReset.height}`);
  const ups = () => p.evaluate(() => window.avdsp.getState().config.chain.upsampler);
  // Preset mode: pick a family, then a variant; Custom keeps its own settings.
  const filterSeg = p.getByRole("group", { name: "Filter design" });
  await filterSeg.getByRole("button", { name: "Preset" }).click();
  await p.waitForFunction(() => !!window.avdsp.getState().config.chain.upsampler.preset, null, { timeout: 2000 }).catch(() => fail("Preset mode did not set a preset"));
  await p.getByRole("group", { name: "Filter family" }).getByRole("button", { name: /^Flat-top/ }).click();
  await p.waitForFunction(() => window.avdsp.getState().config.chain.upsampler.preset === "flattop_long", null, { timeout: 2000 }).catch(() => fail("Flat-top family did not pick flattop_long"));
  await p.getByRole("group", { name: "Flat-top length" }).getByRole("button", { name: "X-long" }).click();
  await p.waitForFunction(() => window.avdsp.getState().config.chain.upsampler.preset === "flattop_xlong_apod", null, { timeout: 2000 }).catch(() => fail("X-long did not keep the apodizing roll-off"));
  await p.getByRole("group", { name: "Flat-top roll-off" }).getByRole("button", { name: "Wide" }).click();
  await p.waitForFunction(() => window.avdsp.getState().config.chain.upsampler.preset === "flattop_xlong", null, { timeout: 2000 }).catch(() => fail("Wide did not pick flattop_xlong"));
  const early = p.getByRole("group", { name: "Flat-top roll-off" }).getByRole("button", { name: "Early" });
  if (!(await early.isDisabled()) || !((await early.getAttribute("title")) ?? "").includes("no x-long Flat-top")) fail("a missing combination (x-long early) is not disabled with a reason");
  if ((await p.locator(".ups-preset-name").textContent()) !== "flattop_xlong") fail("the raw preset name is not shown");
  await p.waitForFunction(() => document.querySelector('.nav-item.active .nav-sub')?.textContent === "Flat-top xlong · linear · ×4", null, { timeout: 2000 }).catch(async () =>
    fail(`sidebar summary for a preset: ${await p.locator(".nav-item.active .nav-sub").textContent()}`),
  );
  await p.waitForSelector(".ups-plots canvas");
  await shot(p, "upsampling-preset");
  // Edit as custom: the preset's exact values play at once as the custom design.
  await p.getByRole("button", { name: "Edit as custom" }).click();
  await p.waitForFunction(() => { const u = window.avdsp.getState().config.chain.upsampler; return u.preset === null && u.design?.based_on === "flattop_xlong"; }, null, { timeout: 2000 }).catch(async () => fail(`Edit as custom did not play the copy: ${JSON.stringify(await ups())}`));
  if (await p.locator(".ups-draft").count()) fail("Edit as custom shows a draft notice");
  await p.waitForFunction(() => document.querySelector(".ups-basis")?.textContent?.startsWith("Same as Flat-top xlong"), null, { timeout: 3000 }).catch(async () => fail(`basis line: ${await p.locator(".ups-basis").textContent()}`));
  if (!(await p.locator(".row", { has: p.getByRole("group", { name: "Filter design" }) }).locator(".ups-basis").count())) fail("the basis note is not on the Filter row");
  await p.getByRole("group", { name: "Phase", exact: true }).getByRole("button", { name: "Minimum" }).click();
  await p.waitForFunction(() => document.querySelector(".ups-basis")?.textContent?.startsWith("Modified from Flat-top xlong"), null, { timeout: 3000 }).catch(async () => fail(`basis line after an edit: ${await p.locator(".ups-basis").textContent()}`));
  await p.waitForFunction(() => { const u = window.avdsp.getState().config.chain.upsampler; return u.preset === null && u.design?.based_on === "flattop_xlong" && u.design.phase.type === "minimum"; }, null, { timeout: 3000 }).catch(async () => fail(`the edit did not play: ${JSON.stringify(await ups())}`));
  await shot(p, "upsampling-edit-as-custom");
  await p.waitForFunction(() => /^Custom Flat-top 4,095\/in · minimum · ×4$/.test(document.querySelector(".nav-item.active .nav-sub")?.textContent ?? ""), null, { timeout: 2000 }).catch(async () =>
    fail(`sidebar summary for a custom design: ${await p.locator(".nav-item.active .nav-sub").textContent()}`),
  );
  // Revert to preset: the name again.
  await p.locator(".ups-basis").getByRole("button", { name: "Revert to preset" }).click();
  await p.waitForFunction(() => window.avdsp.getState().config.chain.upsampler.preset === "flattop_xlong", null, { timeout: 2000 }).catch(() => fail("Revert to preset did not run the preset"));
  // Custom again restores the last custom design.
  await filterSeg.getByRole("button", { name: "Custom" }).click();
  await p.waitForFunction(() => window.avdsp.getState().config.chain.upsampler.design?.phase?.type === "minimum", null, { timeout: 2000 }).catch(() => fail("Custom did not restore the last custom design"));
  await p.evaluate((d) => window.avdsp.setUpsampler({ design: d }), DESIGN);
  await filterSeg.getByRole("button", { name: "Preset" }).click();
  await p.waitForFunction(() => window.avdsp.getState().config.chain.upsampler.preset === "flattop_xlong", null, { timeout: 2000 }).catch(() => fail("Preset did not return to the last preset"));
  // Recommended: one card, the whole design and rate at once; Custom opens it for editing.
  const famGroup = p.getByRole("group", { name: "Filter family" });
  const recCard = famGroup.getByRole("button", { name: /^Recommended/ });
  if (!((await recCard.getAttribute("title")) ?? "").includes("linear-phase")) fail("the Recommended card has no plain tooltip");
  await recCard.click();
  await p
    .waitForFunction(() => { const u = window.avdsp.getState().config.chain.upsampler; return u.preset === "recommended" && u.design === null && JSON.stringify(u.target) === '{"max_rate":768000}'; }, null, { timeout: 3000 })
    .catch(async () => fail(`Recommended did not apply: ${JSON.stringify(await ups())}`));
  await p.waitForFunction(() => document.querySelector('[role=group][aria-label="Filter family"] button[aria-pressed="true"]')?.textContent?.startsWith("Recommended"), null, { timeout: 2000 }).catch(() => fail("the Recommended card is not shown as selected"));
  if ((await filterSeg.getByRole("button", { name: "Preset" }).getAttribute("aria-pressed")) !== "true") fail("Recommended should show in Preset mode");
  await p.waitForFunction(() => /^Recommended · /.test(document.querySelector(".nav-item.active .nav-sub")?.textContent ?? ""), null, { timeout: 2000 }).catch(async () =>
    fail(`sidebar summary for Recommended: ${await p.locator(".nav-item.active .nav-sub").textContent()}`),
  );
  await p.waitForSelector(".ups-plots canvas");
  await p.waitForTimeout(300);
  await shot(p, "upsampling-recommended");
  // Custom plays a custom design at once (the last one, else the default); Preset brings a preset back.
  await filterSeg.getByRole("button", { name: "Custom" }).click();
  await p.waitForFunction(() => { const u = window.avdsp.getState().config.chain.upsampler; return u.preset === null && u.design !== null; }, null, { timeout: 2000 }).catch(async () => fail(`Custom did not play a design: ${JSON.stringify(await ups())}`));
  await shot(p, "upsampling-recommended-custom");
  await filterSeg.getByRole("button", { name: "Preset" }).click();
  await p.waitForFunction(() => window.avdsp.getState().config.chain.upsampler.preset !== null, null, { timeout: 2000 }).catch(() => fail("Preset did not run a preset again"));
  await p.evaluate(() => window.avdsp.setUpsampler({ preset: "flattop_xlong", target: { ratio: 4 } }));
  await p.waitForFunction(() => window.avdsp.getState().config.chain.upsampler.preset === "flattop_xlong", null, { timeout: 2000 }).catch(() => fail("could not restore the preset after Recommended"));
  // A fixed output rate: integer-only families are disabled with the reason; a refusal shows inline.
  await p.getByRole("group", { name: "Filter family" }).getByRole("button", { name: /^Gaussian/ }).click();
  await p.waitForFunction(() => window.avdsp.getState().config.chain.upsampler.preset === "gaussian_long", null, { timeout: 2000 }).catch(() => fail("Gaussian family did not pick gaussian_long"));
  await p.getByRole("group", { name: "Rate", exact: true }).getByRole("button", { name: "Fixed rate" }).click();
  await p.getByRole("group", { name: "Output rate" }).getByRole("button", { name: "48k", exact: true }).click();
  await p.waitForFunction(() => JSON.stringify(window.avdsp.getState().config.chain.upsampler.target) === '{"rate":48000}', null, { timeout: 2000 }).catch(() => fail("could not set a fixed 48 kHz output"));
  const blockFam = p.getByRole("group", { name: "Filter family" }).getByRole("button", { name: /^Block FFT/ });
  if (!(await blockFam.isDisabled()) || !((await blockFam.getAttribute("title")) ?? "").includes("160/147")) fail("Block FFT is not disabled at 44.1 → 48 kHz");
  // The reason is reachable by keyboard: the card stays in the tab order, describes itself, and shows it on focus.
  await p.evaluate(() => {
    const cards = [...document.querySelectorAll('[role=group][aria-label="Filter family"] button')];
    const i = cards.findIndex((b) => b.textContent?.startsWith("Block FFT"));
    (cards[i - 1] ?? document.body).focus();
  });
  await p.keyboard.press("Tab");
  const focusWhy = await p.evaluate(() => {
    const a = document.activeElement;
    const tip = a?.getAttribute("aria-describedby") ? document.getElementById(a.getAttribute("aria-describedby")) : null;
    return { name: a?.textContent ?? "", why: tip?.textContent ?? "", width: tip?.getBoundingClientRect().width ?? 0 };
  });
  if (!focusWhy.name.startsWith("Block FFT") || !focusWhy.why.includes("160/147") || focusWhy.width < 40) fail(`a disabled family's reason is not reachable by keyboard: ${JSON.stringify(focusWhy)}`);
  await shot(p, "upsampling-disabled-reason");
  await blockFam.press("Enter");
  await p.waitForTimeout(200);
  if ((await ups()).preset === "block_fft") fail("an unavailable family card still applies on Enter");
  await p.evaluate(() => window.avdsp.setUpsampler({ preset: "block_fft" }));
  await p.waitForSelector(".ups-engine-err", { timeout: 2000 }).catch(() => fail("the engine's refusal is not shown inline"));
  if (!((await p.locator(".ups-engine-err").textContent()) ?? "").includes("does not run at 160/147")) fail(`inline error text: ${await p.locator(".ups-engine-err").textContent()}`);
  await p.getByRole("group", { name: "Rate", exact: true }).getByRole("button", { name: "Multiply" }).click();
  await p.waitForFunction(() => JSON.stringify(window.avdsp.getState().config.chain.upsampler.target) === '{"ratio":4}', null, { timeout: 2000 });
  await p.waitForFunction(() => !document.querySelector(".ups-engine-err"), null, { timeout: 2000 }).catch(() => fail("the inline error did not clear after a good edit"));
  // DSP overload: the engine's fallback shows as plain text above the
  // settings, a design too heavy for the CPU is refused inline, and a new
  // upsampler setting ends the fallback.
  await p.evaluate(() => window.avdspShell.daemon.simulateOverload(1.4));
  await p.waitForSelector(".ups-overload", { timeout: 2000 }).catch(() => fail("the overload fallback is not shown"));
  const overloadText = (await p.locator(".ups-overload").textContent()) ?? "";
  if (!overloadText.startsWith("The engine switched the upsampler off") || !overloadText.includes("load 140%") || !overloadText.includes("Your setting is kept")) fail(`overload text: ${overloadText}`);
  // The plots then show the saved setting, not what plays.
  await p.waitForFunction(() => [...document.querySelectorAll(".ups-badge")].some((b) => b.textContent === "Saved setting"), null, { timeout: 4000 }).catch(() => fail("the plot is not labelled as the saved setting during the fallback"));
  if ([...(await p.locator(".ups-badge").allTextContents())].includes("Playing now")) fail("the plot says Playing now during the fallback");
  await p.evaluate((d) => window.avdsp.setUpsampler({ design: { ...d, length: { rule: "fixed", taps: 4194304 } }, target: { ratio: 4 } }), DESIGN);
  await p.waitForSelector(".ups-engine-err", { timeout: 2000 }).catch(() => fail("the CPU refusal is not shown inline"));
  const cpuErr = (await p.locator(".ups-engine-err").textContent()) ?? "";
  if (!cpuErr.includes("Too heavy for this CPU at") || !cpuErr.includes("Use fewer taps or a lower output rate.")) fail(`CPU refusal text: ${cpuErr}`);
  if (await p.locator(".ups-overload").count() !== 1) fail("a refused change ended the overload fallback");
  await shot(p, "upsampling-overload");
  // Any change of the upsampler setting (here its target) ends it.
  const upsTarget = (await ups()).target;
  await p.evaluate(() => window.avdsp.setUpsampler({ target: { ratio: 2 } }));
  await p.waitForFunction(() => !document.querySelector(".ups-overload"), null, { timeout: 2000 }).catch(() => fail("a new upsampler setting did not end the overload fallback"));
  await p.evaluate((t) => window.avdsp.setUpsampler({ target: t }), upsTarget);
  await p.waitForFunction((t) => JSON.stringify(window.avdsp.getState().config.chain.upsampler.target) === JSON.stringify(t), upsTarget, { timeout: 2000 });
  await p.waitForFunction(() => !document.querySelector(".ups-engine-err"), null, { timeout: 2000 }).catch(() => fail("the CPU refusal did not clear after a good edit"));
  // Multiply offers every whole ratio the engine takes, Highest every cap.
  const target = () => p.evaluate(() => JSON.stringify(window.avdsp.getState().config.chain.upsampler.target));
  const waitTarget = (t, what) => p.waitForFunction((t) => JSON.stringify(window.avdsp.getState().config.chain.upsampler.target) === t, t, { timeout: 2000 }).catch(async () => fail(`${what}: target ${await target()}`));
  const field = (lbl) => p.locator(".field", { has: p.locator(".lbl", { hasText: new RegExp(`^${lbl}$`) }) }).locator("input");
  await p.getByRole("group", { name: "Ratio" }).getByRole("button", { name: "×3", exact: true }).click();
  await waitTarget('{"ratio":3}', "×3");
  await field("Other ×").fill("5");
  await field("Other ×").press("Enter");
  await waitTarget('{"ratio":5}', "Other × 5");
  await p.getByRole("group", { name: "Rate", exact: true }).getByRole("button", { name: "Highest" }).click();
  await p.getByRole("group", { name: "Up to" }).getByRole("button", { name: "96k", exact: true }).click();
  await waitTarget('{"max_rate":96000}', "Up to 96k");
  await field("Other").fill("120");
  await field("Other").press("Enter");
  await waitTarget('{"max_rate":120000}', "Up to 120 kHz");
  await p.getByRole("group", { name: "Rate", exact: true }).getByRole("button", { name: "Multiply" }).click();
  await waitTarget('{"ratio":5}', "Multiply returns to ×5");
  await p.getByRole("group", { name: "Ratio" }).getByRole("button", { name: "×4", exact: true }).click();
  await waitTarget('{"ratio":4}', "×4");
  // A preset at a fixed rate, and a custom design that does not fit it: the
  // preset keeps playing while the design is fixed, then the fixed design plays.
  await p.evaluate((d) => window.avdsp.setUpsampler({ design: d }), DESIGN);
  await p.waitForFunction(() => window.avdsp.getState().config.chain.upsampler.design?.length?.taps === 65536, null, { timeout: 2000 });
  await filterSeg.getByRole("button", { name: "Preset" }).click();
  await p.waitForFunction(() => window.avdsp.getState().config.chain.upsampler.preset !== null, null, { timeout: 2000 });
  await p.getByRole("group", { name: "Rate", exact: true }).getByRole("button", { name: "Fixed rate" }).click();
  await waitTarget('{"rate":48000}', "Fixed rate returns to 48 kHz");
  const presetNow = (await ups()).preset;
  await filterSeg.getByRole("button", { name: "Custom" }).click();
  await p.waitForSelector(".ups-draft", { timeout: 2000 }).catch(() => fail("a refused Custom does not open the draft"));
  if ((await ups()).preset !== presetNow) fail(`the preset stopped while drafting: ${(await ups()).preset}`);
  await p.waitForSelector(".ups-field-err", { timeout: 3000 }).catch(() => fail("the draft does not show why the design does not fit"));
  if (!((await p.locator(".ups-draft").textContent()) ?? "").startsWith("Not applied")) fail(`the draft notice: ${await p.locator(".ups-draft").textContent()}`);
  if (await p.locator(".ups-draft").getByRole("button", { name: /Apply/ }).count()) fail("the draft notice still offers an Apply step");
  await shot(p, "upsampling-draft");
  await p.getByRole("group", { name: "Length rule" }).getByRole("button", { name: "Per input sample" }).click();
  await p
    .waitForFunction(() => { const u = window.avdsp.getState().config.chain.upsampler; return u.preset === null && u.design?.length?.rule === "per_input" && u.target.rate === 48000; }, null, { timeout: 2000 })
    .catch(async () => fail(`the fixed design did not play: ${JSON.stringify(await ups())}`));
  await p.waitForFunction(() => !document.querySelector(".ups-draft") && !document.querySelector(".ups-field-err"), null, { timeout: 3000 }).catch(() => fail("the notice or the error stays after the fix"));
  // Custom: picking a fractional rate the design does not fit holds it back with the reason.
  await p.evaluate((d) => window.avdsp.setUpsampler({ target: { ratio: 4 }, design: d }), DESIGN);
  await waitTarget('{"ratio":4}', "back to ×4");
  await p.getByRole("group", { name: "Rate", exact: true }).getByRole("button", { name: "Fixed rate" }).click();
  await p.waitForSelector(".ups-offer", { timeout: 2000 }).catch(() => fail("a fractional rate the design does not fit is not held back"));
  await p.waitForTimeout(200);
  if ((await target()) !== '{"ratio":4}') fail(`a fractional rate the design does not fit was sent: ${await target()}`);
  if (!((await p.locator(".ups-offer").textContent()) ?? "").includes("160/147")) fail(`the hold-back reason: ${await p.locator(".ups-offer").textContent()}`);
  await p.locator(".ups-offer").getByRole("button", { name: "Cancel" }).click();
  await p.close();

  // --- a fresh page: upsampling off, then Best for DAC
  p = await page();
  await p.waitForSelector(".app");
  await p.locator(".nav-main", { hasText: "Upsampling" }).click();
  await p.waitForSelector(".ups-plots canvas");
  // Upsampling off while 44.1 kHz plays through the DSP: 1:1, and the plots preview the filter.
  await p.evaluate(() => window.avdsp.setUpsampler({ target: "off" }));
  await p.waitForFunction(() => window.avdsp.getState().status?.rate_ratio?.up === 1 && window.avdsp.getState().status?.graph_rate === 44100, null, { timeout: 3000 }).catch(() => fail("off: the engine still resamples"));
  await p.waitForFunction(() => document.querySelector(".ups-badge")?.textContent === "Target preview", null, { timeout: 3000 }).catch(async () => fail(`off: the plots are not a preview: ${await p.locator(".ups-badge").textContent()}`));
  // Best for DAC, remembered in this browser for the next engine.
  await p.getByRole("group", { name: "Rate", exact: true }).getByRole("button", { name: "Highest" }).click();
  if (!(await p.locator(".row", { has: p.getByRole("group", { name: "Rate", exact: true }) }).getByRole("group", { name: "Rate family" }).count())) fail("the rate family switch is not on the Rate row");
  await p.getByRole("group", { name: "Rate family" }).getByRole("button", { name: "Best for DAC" }).click();
  await p.waitForFunction(() => "max_rate_any" in (window.avdsp.getState().config.chain.upsampler.target ?? {}), null, { timeout: 2000 }).catch(() => fail("could not pick Best for DAC"));
  await p.getByRole("group", { name: "Rate", exact: true }).getByRole("button", { name: "Multiply" }).click();
  await p.waitForFunction(() => "ratio" in (window.avdsp.getState().config.chain.upsampler.target ?? {}), null, { timeout: 2000 });

  // --- an engine from before fractional rates (no rate_ratio): the view opens, new targets are off
  {
    const prev = p;
    p = await prev.context().newPage();
    p.on("pageerror", (e) => fail(`pageerror: ${e.message}`));
    await p.goto(url + "?legacy_ratio");
    await prev.close();
  }
  await p.waitForSelector(".app");
  await p.locator(".nav-main", { hasText: "Upsampling" }).click();
  await p.waitForSelector(".ups-plots canvas", { timeout: 5000 }).catch(() => fail("legacy engine: the Upsampling view did not render"));
  const fixedBtn = p.getByRole("group", { name: "Rate", exact: true }).getByRole("button", { name: "Fixed rate" });
  if (!(await fixedBtn.isDisabled()) || !((await fixedBtn.getAttribute("title")) ?? "").includes("update the engine")) fail("legacy engine: Fixed rate is offered");
  await p.getByRole("group", { name: "Rate", exact: true }).getByRole("button", { name: "Highest" }).click();
  const anyFam = p.getByRole("group", { name: "Rate family" }).getByRole("button", { name: "Best for DAC" });
  await anyFam.waitFor({ timeout: 2000 }).catch(() => fail("legacy engine: Highest did not open"));
  if (!(await anyFam.isDisabled())) fail("legacy engine: Best for DAC is offered");
  // The Best for DAC remembered from the newer engine comes back in the integer form.
  await p
    .waitForFunction(() => JSON.stringify(window.avdsp.getState().config.chain.upsampler.target) === '{"max_rate":768000}', null, { timeout: 2000 })
    .catch(async () => fail(`legacy engine: Highest restored ${await p.evaluate(() => JSON.stringify(window.avdsp.getState().config.chain.upsampler.target))}`));
  if (await p.locator(".ups-engine-err").count()) fail(`legacy engine: ${await p.locator(".ups-engine-err").textContent()}`);
  await p.close();

  // --- a 48 kHz-only DAC: a fixed 48 kHz output resamples 44.1 kHz by 160/147, with a note
  p = await page("?device=SPK", "light");
  await p.waitForSelector(".app");
  await p.locator(".nav-main", { hasText: "Upsampling" }).click();
  // A long output-rate design does not fit 160/147: count the length per input sample.
  await p.evaluate(() => window.avdsp.setUpsampler({ design: { ...window.avdsp.getState().config.chain.upsampler.design, length: { rule: "per_input", taps: 4096 } } }));
  await p.getByRole("group", { name: "Rate", exact: true }).getByRole("button", { name: "Fixed rate" }).click();
  await p.getByRole("group", { name: "Output rate" }).getByRole("button", { name: "48k", exact: true }).click();
  await p.waitForFunction(() => JSON.stringify(window.avdsp.getState().config.chain.upsampler.target) === '{"rate":48000}', null, { timeout: 2000 }).catch(() => fail("SPK: could not set a fixed 48 kHz output"));
  await p.waitForFunction(() => window.avdsp.getState().status?.rate_ratio?.down === 147, null, { timeout: 3000 }).catch(() => fail("SPK: the engine does not run 160/147"));
  await p.waitForFunction(() => document.querySelector(".ups-landing")?.textContent?.includes("cannot play 44.1 kHz"), null, { timeout: 3000 }).catch(async () =>
    fail(`SPK: no note that the DAC cannot play 44.1 kHz: ${await p.locator(".ups-landing").textContent()}`),
  );
  // What runs now is the status bar's (the page has no line of its own for it).
  if (await p.locator(".ups-landing-now").count()) fail("SPK: the page still shows a Now line");
  if (!((await p.locator(".status .pill.ratio").textContent()) ?? "").includes("160/147")) fail("SPK: the status bar does not show 160/147");
  await p.waitForSelector(".ups-plots canvas");
  // The plots show what plays.
  await p.waitForFunction(() => document.querySelector(".ups-badge")?.textContent === "Playing now", null, { timeout: 3000 }).catch(async () => fail(`SPK: the plots are not the live filter: ${await p.locator(".ups-badge").textContent()}`));
  await p.waitForTimeout(400);
  await shot(p, "upsampling-fractional-light");
  await p.getByRole("group", { name: "Preview at source rate" }).getByRole("button", { name: "96k", exact: true }).click();
  await p.waitForFunction(() => document.querySelector(".ups-badge")?.textContent === "Target preview", null, { timeout: 3000 }).catch(() => fail("SPK: another preview rate is not labelled a target preview"));
  await p.getByRole("group", { name: "Preview at source rate" }).getByRole("button", { name: "44.1k", exact: true }).click();
  // Presets at 44.1 → 48 kHz: integer-only families are off, the rest run.
  await p.getByRole("group", { name: "Filter design" }).getByRole("button", { name: "Preset" }).click();
  await p.waitForFunction(() => window.avdsp.getState().config.chain.upsampler.preset !== null, null, { timeout: 2000 }).catch(() => fail("SPK: Preset did not go back to a preset"));
  if (!(await p.getByRole("group", { name: "Filter family" }).getByRole("button", { name: /^Cascade/ }).isDisabled())) fail("SPK: Cascade is not disabled at 160/147");
  await p.waitForSelector(".ups-plots canvas");
  await p.waitForTimeout(400);
  await shot(p, "upsampling-preset-light");
  await p.close();

  // --- Device: "Also play on" (multi-device followers)
  for (const skin of ["midnight", "light"]) {
    p = await page("?followers=USB,SPK", skin);
    await p.waitForSelector(".app");
    await p.locator(".side .nav-main", { hasText: "Device" }).click();
    const panel = p.locator(".panel.followers");
    await panel.waitFor({ timeout: 3000 }).catch(() => fail(`followers (${skin}): the "Also play on" panel is missing`));
    await panel.scrollIntoViewIfNeeded();
    await panel.locator(".follower-chip", { hasText: "In sync" }).waitFor({ timeout: 3000 }).catch(() => fail(`followers (${skin}): no "In sync" chip`));
    await panel.locator(".follower-chip", { hasText: "ms late" }).waitFor({ timeout: 3000 }).catch(() => fail(`followers (${skin}): no "ms late" chip for the unaligned follower`));
    if (!(await panel.textContent())?.includes("1.996")) fail(`followers (${skin}): no speed cap note`);
    if (skin === "midnight") {
      await panel.getByLabel("Device to add").selectOption("hw:CARD=PCH,DEV=0");
      await panel.getByRole("button", { name: "Add device" }).click();
      await p
        .waitForFunction(() => window.avdsp.getState().config?.outputs.followers.length === 3, null, { timeout: 3000 })
        .catch(() => fail(`followers: adding Built-in Audio did not apply`));
      await panel.locator(".follower-card").nth(2).locator(".follower-chip", { hasText: /Opening|Locking/ }).waitFor({ timeout: 3000 }).catch(() => fail("followers: the new follower shows no opening/locking chip"));
      if (!(await panel.getByLabel("Device to add").isDisabled())) fail("followers: the picker is still enabled with 3 followers");
      await p.locator(".follower-card").nth(2).getByRole("button", { name: /^Play on / }).click();
      await p
        .waitForFunction(() => window.avdsp.getState().config?.outputs.followers[2]?.enabled === false, null, { timeout: 3000 })
        .catch(() => fail("followers: the power button did not disable the follower"));
    }
    await p.waitForTimeout(300);
    await panel.screenshot({ path: resolve(out, `device-followers-${skin}.png`) });
    if (skin === "midnight") {
      // Moving the main device onto a follower is refused, with an offer to remove it first.
      await p.locator("#device-page").selectOption("hw:CARD=USB,DEV=0");
      const conflict = p.locator(".device-conflict");
      await conflict.waitFor({ timeout: 3000 }).catch(() => fail("followers: no note when the main device moves onto a follower"));
      await conflict.scrollIntoViewIfNeeded();
      await shot(p, "device-followers-conflict");
      await conflict.getByRole("button", { name: "Remove it and switch" }).click();
      await p
        .waitForFunction(() => window.avdsp.getState().config?.device === "hw:CARD=USB,DEV=0" && window.avdsp.getState().config?.outputs.followers.length === 2, null, { timeout: 3000 })
        .catch(async () => fail(`followers: "Remove it and switch" did not apply: ${await p.evaluate(() => JSON.stringify(window.avdsp.getState().config?.outputs))}`));
    }
    await p.close();
  }

  // --- a 48 kHz-only DAC, Device: who converts a rate the DAC lacks for Direct and DSP off
  for (const skin of ["midnight", "light"]) {
    p = await page("?device=USB48", skin);
    await p.waitForSelector(".app");
    await p.evaluate(() => window.avdsp.routeStream(window.avdsp.getState().streams.find((s) => s.node === 81), "direct"));
    await p.locator(".side .nav-main", { hasText: "Device" }).click();
    const who = p.getByRole("group", { name: "Who converts the rate for Direct and DSP off when the DAC lacks it" });
    const engineBtn = who.getByRole("button", { name: "Engine", exact: true });
    const pwBtn = who.getByRole("button", { name: "PipeWire", exact: true });
    await engineBtn.waitFor({ timeout: 3000 }).catch(() => fail(`bypass resampler (${skin}): the Device option is missing`));
    if ((await engineBtn.getAttribute("aria-pressed")) !== "true") fail(`bypass resampler (${skin}): Engine is not the default`);
    if (!((await engineBtn.getAttribute("title")) ?? "").includes("Not bit-perfect")) fail(`bypass resampler (${skin}): Engine tooltip: ${await engineBtn.getAttribute("title")}`);
    if (!((await pwBtn.getAttribute("title")) ?? "").includes("PipeWire moves the playback")) fail(`bypass resampler (${skin}): PipeWire tooltip: ${await pwBtn.getAttribute("title")}`);
    await p
      .waitForFunction(() => [...document.querySelectorAll(".hint")].some((h) => /The engine converts 44\.1 kHz to 48 kHz now/.test(h.textContent ?? "")), null, { timeout: 3000 })
      .catch(async () => fail(`bypass resampler (${skin}): no note that the engine converts 44.1 kHz: ${JSON.stringify(await p.evaluate(() => window.avdsp.getState().status?.bypass_resampler))}`));
    const ratio = await p.evaluate(() => window.avdsp.getState().status?.rate_ratio);
    if (ratio?.up !== 160 || ratio?.down !== 147) fail(`bypass resampler (${skin}): Direct does not run 160/147: ${JSON.stringify(ratio)}`);
    await p.waitForTimeout(300);
    await shot(p, `device-bypass-resampler-${skin}`);
    await pwBtn.click();
    await p
      .waitForFunction(() => window.avdsp.getState().config?.rate.bypass_resampler === "pipewire" && window.avdsp.getState().status?.graph_rate === 48000, null, { timeout: 3000 })
      .catch(async () => fail(`bypass resampler (${skin}): PipeWire did not apply: ${await p.evaluate(() => JSON.stringify([window.avdsp.getState().config?.rate, window.avdsp.getState().status?.graph_rate]))}`));
    if ((await pwBtn.getAttribute("aria-pressed")) !== "true") fail(`bypass resampler (${skin}): PipeWire is not shown selected`);
    await p.close();
  }

  // --- x32 / x64: the 1.5M and 3M caps and the ×32 / ×64 multiples. A DAC
  // that stops at 768 kHz (the default mock DAC) shows them disabled with
  // the reason, reachable by keyboard; a hi-res DAC to 3.072 MHz runs them.
  for (const skin of ["midnight", "light"]) {
    p = await page("", skin);
    await p.waitForSelector(".app");
    await p.locator(".nav-main", { hasText: "Upsampling" }).click();
    const tgt = () => p.evaluate(() => JSON.stringify(window.avdsp.getState().config.chain.upsampler.target));
    await p.getByRole("group", { name: "Rate", exact: true }).getByRole("button", { name: "Highest" }).click();
    await p.waitForFunction(() => "max_rate" in (window.avdsp.getState().config.chain.upsampler.target ?? {}), null, { timeout: 2000 }).catch(() => fail("768k DAC: Highest did not open"));
    const upTo = p.getByRole("group", { name: "Up to" });
    for (const [label, why] of [
      ["1.5M", "Your DAC does not report 1411.2 kHz or 1536 kHz"],
      ["3M", "Your DAC does not report 2822.4 kHz or 3072 kHz"],
    ]) {
      const b = upTo.getByRole("button", { name: label, exact: true });
      if ((await b.getAttribute("aria-disabled")) !== "true") fail(`768k DAC: Up to ${label} is not disabled`);
      if ((await b.getAttribute("title")) !== why) fail(`768k DAC: Up to ${label} reason: ${await b.getAttribute("title")}`);
      const before = await tgt();
      await b.click({ force: true });
      await p.waitForTimeout(150);
      if ((await tgt()) !== before) fail(`768k DAC: a disabled Up to ${label} was sent: ${await tgt()}`);
    }
    // Keyboard: Tab from 768k lands on 1.5M, which describes itself and shows why.
    await upTo.getByRole("button", { name: "768k", exact: true }).focus();
    await p.keyboard.press("Tab");
    const focused = await p.evaluate(() => {
      const a = document.activeElement;
      const tip = a?.getAttribute("aria-describedby") ? document.getElementById(a.getAttribute("aria-describedby")) : null;
      return { name: a?.textContent ?? "", why: tip?.textContent ?? "", width: tip?.getBoundingClientRect().width ?? 0 };
    });
    if (focused.name !== "1.5M" || !focused.why.includes("does not report 1411.2 kHz") || focused.width < 40) fail(`768k DAC: the 1.5M reason is not reachable by keyboard: ${JSON.stringify(focused)}`);
    await shot(p, `upsampling-high-rates-disabled-${skin}`);
    // The Other field takes any limit up to 3072 kHz (the engine lands where the DAC plays).
    const other = p.locator(".field", { has: p.locator(".lbl", { hasText: /^Other$/ }) }).locator("input");
    await other.fill("5000");
    await other.press("Enter");
    await p.waitForFunction(() => JSON.stringify(window.avdsp.getState().config.chain.upsampler.target) === '{"max_rate":3072000}', null, { timeout: 2000 }).catch(async () => fail(`768k DAC: Other 5000 kHz is not clamped to 3072: ${await tgt()}`));
    await p.waitForFunction(() => (window.avdsp.getState().status?.rate_ratio?.up ?? 0) === 16, null, { timeout: 3000 }).catch(async () => fail(`768k DAC: a 3072 kHz cap does not land at ×16: ${JSON.stringify(await p.evaluate(() => window.avdsp.getState().status?.rate_ratio))}`));
    await p.getByRole("group", { name: "Rate", exact: true }).getByRole("button", { name: "Multiply" }).click();
    const ratioGrp = p.getByRole("group", { name: "Ratio" });
    for (const n of [32, 64]) {
      if ((await ratioGrp.getByRole("button", { name: `×${n}`, exact: true }).getAttribute("aria-disabled")) !== "true") fail(`768k DAC: ×${n} is not disabled`);
    }
    if ((await ratioGrp.getByRole("button", { name: "×16", exact: true }).getAttribute("aria-disabled")) === "true") fail("768k DAC: ×16 is disabled");
    const rowBox = await ratioGrp.boundingBox();
    const panelBox = await p.locator(".panel").first().boundingBox();
    if (rowBox && panelBox && rowBox.x + rowBox.width > panelBox.x + panelBox.width + 0.5) fail("the Multiply row overflows its panel");
    await p.close();

    p = await page("?device=USB", skin);
    await p.waitForSelector(".app");
    await p.locator(".nav-main", { hasText: "Upsampling" }).click();
    await p.getByRole("group", { name: "Rate", exact: true }).getByRole("button", { name: "Highest" }).click();
    const hiUpTo = p.getByRole("group", { name: "Up to" });
    for (const label of ["1.5M", "3M"]) {
      if ((await hiUpTo.getByRole("button", { name: label, exact: true }).getAttribute("aria-disabled")) === "true") fail(`hi-res DAC: Up to ${label} is disabled`);
    }
    await hiUpTo.getByRole("button", { name: "3M", exact: true }).click();
    await p.waitForFunction(() => JSON.stringify(window.avdsp.getState().config.chain.upsampler.target) === '{"max_rate":3072000}', null, { timeout: 2000 }).catch(async () => fail(`hi-res DAC: Up to 3M: ${await tgt()}`));
    await p.waitForFunction(() => (window.avdsp.getState().status?.rate_ratio?.up ?? 0) === 64, null, { timeout: 3000 }).catch(async () => fail(`hi-res DAC: 3M does not land at ×64: ${JSON.stringify(await p.evaluate(() => window.avdsp.getState().status?.rate_ratio))}`));
    await p.waitForFunction(() => document.querySelector(".status .pill.ratio")?.textContent === "×64", null, { timeout: 3000 }).catch(async () => fail(`hi-res DAC: status bar ratio: ${await p.locator(".status .path").textContent()}`));
    await p.waitForSelector(".ups-plots canvas", { timeout: 5000 }).catch(() => fail("hi-res DAC: no response plot at ×64"));
    if (await p.locator(".ups-err").count()) fail(`hi-res DAC: the ×64 response failed: ${await p.locator(".ups-err").textContent()}`);
    await p.waitForTimeout(400);
    await shot(p, `upsampling-high-rates-enabled-${skin}`);
    await p.getByRole("group", { name: "Rate", exact: true }).getByRole("button", { name: "Multiply" }).click();
    await p.getByRole("group", { name: "Ratio" }).getByRole("button", { name: "×32", exact: true }).click();
    await p.waitForFunction(() => JSON.stringify(window.avdsp.getState().config.chain.upsampler.target) === '{"ratio":32}', null, { timeout: 2000 }).catch(async () => fail(`hi-res DAC: ×32: ${await tgt()}`));
    await p.getByRole("group", { name: "Ratio" }).getByRole("button", { name: "×64", exact: true }).click();
    await p.waitForFunction(() => JSON.stringify(window.avdsp.getState().config.chain.upsampler.target) === '{"ratio":64}', null, { timeout: 2000 }).catch(async () => fail(`hi-res DAC: ×64: ${await tgt()}`));
    await shot(p, `upsampling-multiply-x64-${skin}`);
    // Fixed rate lists the high rates. 2822.4 kHz is reachable from every
    // allowed rate (×64 from 44.1 kHz, 294/5 from 48 kHz); 3072 kHz from
    // 44.1 kHz takes 10240/147, past the 4096 term limit: the engine refuses
    // it and the note beside the control says so, with the engine's words.
    await p.getByRole("group", { name: "Rate", exact: true }).getByRole("button", { name: "Fixed rate" }).click();
    const outRate = p.getByRole("group", { name: "Output rate" });
    for (const label of ["1411.2k", "1536k", "2822.4k", "3072k"]) {
      if ((await outRate.getByRole("button", { name: label, exact: true }).getAttribute("aria-disabled")) === "true") fail(`hi-res DAC: Fixed ${label} is disabled`);
    }
    if (!((await outRate.getByRole("button", { name: "3072k", exact: true }).getAttribute("title")) ?? "").includes("not from 44.1 kHz")) fail("hi-res DAC: the 3072k tip does not say 44.1 kHz cannot reach it");
    await outRate.getByRole("button", { name: "2822.4k", exact: true }).click();
    // The custom 65,536-tap design is too long for 294/5: held back; a length per input sample fits.
    await p.waitForSelector(".ups-offer", { timeout: 2000 }).catch(() => fail("hi-res DAC: a design too long for 294/5 is not held back"));
    await p.locator(".ups-offer").getByRole("button", { name: "Cancel" }).click();
    await p.evaluate(() => window.avdsp.setUpsampler({ design: { ...window.avdsp.getState().config.chain.upsampler.design, length: { rule: "per_input", taps: 64 } } }));
    await p.waitForFunction(() => window.avdsp.getState().config.chain.upsampler.design?.length?.rule === "per_input", null, { timeout: 2000 });
    await outRate.getByRole("button", { name: "2822.4k", exact: true }).click();
    await p.waitForFunction(() => JSON.stringify(window.avdsp.getState().config.chain.upsampler.target) === '{"rate":2822400}', null, { timeout: 2000 }).catch(async () => fail(`hi-res DAC: Fixed 2822.4k: ${await tgt()}`));
    if (await p.locator(".ups-term-note").count()) fail(`hi-res DAC: a term note for 2822.4k: ${await p.locator(".ups-term-note").textContent()}`);
    await outRate.getByRole("button", { name: "3072k", exact: true }).click();
    await p.waitForSelector(".ups-term-note .hint", { timeout: 2000 }).catch(async () => fail(`hi-res DAC: no term note with the engine's refusal for 3072k: ${JSON.stringify(await p.evaluate(() => window.avdsp.getState().upsamplerError))} ${await p.locator(".ups-offer").textContent().catch(() => "")}`));
    const note = (await p.locator(".ups-term-note").textContent()) ?? "";
    if (!note.includes("3072 kHz cannot be reached from 44.1 kHz or 88.2 kHz") || !note.includes("Use 2822.4 kHz") || !note.includes("has a term above 4096")) fail(`hi-res DAC: term note: ${note}`);
    if (await p.locator(".ups-engine-err").count()) fail("hi-res DAC: the term refusal is shown twice");
    if ((await tgt()) !== '{"rate":2822400}') fail(`hi-res DAC: a refused 3072k changed the target: ${await tgt()}`);
    await shot(p, `upsampling-fixed-3072-refused-${skin}`);
    // S16 only: the DAC takes the high rates in S32 alone, so the engine
    // would never open them; the buttons say so in the format's name.
    await p.evaluate(() => window.avdsp.editField("set_formats", (c) => c.formats, () => ["s16"], (c, formats) => ({ ...c, formats })));
    await p.waitForFunction(() => JSON.stringify(window.avdsp.getState().config.formats) === '["s16"]', null, { timeout: 2000 }).catch(() => fail("formats S16 not applied"));
    const s16Why = async (b, want) => {
      if ((await b.getAttribute("aria-disabled")) !== "true") fail(`S16: ${want} is not disabled`);
      if ((await b.getAttribute("title")) !== want) fail(`S16 reason: ${await b.getAttribute("title")}`);
    };
    await s16Why(outRate.getByRole("button", { name: "3072k", exact: true }), "Your DAC does not take 3072 kHz in S16");
    // The current pick stays pressed (what is set) but is marked
    // unavailable with its reason, by keyboard too, and a note says the
    // engine lands lower.
    const pickedOff = async (b, want, landing) => {
      if ((await b.getAttribute("aria-pressed")) !== "true") fail(`S16: the selected ${want} is not shown as selected`);
      await s16Why(b, want);
      await b.focus();
      await p.keyboard.press("Shift+Tab");
      await p.keyboard.press("Tab");
      const tip = await p.evaluate(() => {
        const a = document.activeElement;
        const id = a?.getAttribute("aria-describedby");
        const el = id ? document.getElementById(id) : null;
        return { text: el?.textContent ?? "", width: el?.getBoundingClientRect().width ?? 0 };
      });
      if (tip.text !== want || tip.width < 40) fail(`S16: the selected button's reason is not reachable by keyboard: ${JSON.stringify(tip)}`);
      await p.waitForFunction((re) => new RegExp(re).test(document.querySelector(".ups-lands-lower")?.textContent ?? ""), landing.source, { timeout: 3000 }).catch(async () => fail(`S16: lands-lower note: ${await p.locator(".ups-lands-lower").textContent().catch(() => "none")}`));
    };
    await pickedOff(outRate.getByRole("button", { name: "2822.4k", exact: true }), "Your DAC does not take 2822.4 kHz in S16", /^Your DAC does not take 2822\.4 kHz in S16, so the engine lands lower/);
    await p.getByRole("group", { name: "Rate", exact: true }).getByRole("button", { name: "Highest" }).click();
    const s16Cap = p.getByRole("group", { name: "Up to" }).getByRole("button", { name: "1.5M", exact: true });
    await s16Why(s16Cap, "Your DAC does not take 1411.2 kHz or 1536 kHz in S16");
    await p.waitForFunction(() => "max_rate" in (window.avdsp.getState().config.chain.upsampler.target ?? {}), null, { timeout: 2000 }).catch(async () => fail(`S16: Highest did not open: ${await tgt()}`));
    await pickedOff(p.getByRole("group", { name: "Up to" }).getByRole("button", { name: "3M", exact: true }), "Your DAC does not take 2822.4 kHz or 3072 kHz in S16", /so the engine lands lower: (705\.6|768) kHz now\.$/);
    await shot(p, `upsampling-selected-unavailable-${skin}`);
    const beforeS16 = await tgt();
    await s16Cap.click({ force: true });
    await p.waitForTimeout(150);
    if ((await tgt()) !== beforeS16) fail(`S16: a disabled 1.5M was sent: ${await tgt()}`);
    await p.getByRole("group", { name: "Rate", exact: true }).getByRole("button", { name: "Multiply" }).click();
    await s16Why(p.getByRole("group", { name: "Ratio" }).getByRole("button", { name: "×32", exact: true }), "Your DAC does not take 1411.2 kHz or 1536 kHz in S16");
    await p.waitForFunction(() => JSON.stringify(window.avdsp.getState().config.chain.upsampler.target) === '{"ratio":64}', null, { timeout: 2000 }).catch(async () => fail(`S16: Multiply did not restore ×64: ${await tgt()}`));
    await pickedOff(p.getByRole("group", { name: "Ratio" }).getByRole("button", { name: "×64", exact: true }), "Your DAC does not take 2822.4 kHz or 3072 kHz in S16", /so the engine lands lower/);
    await shot(p, `upsampling-high-rates-s16-${skin}`);
    await p.close();
  }

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
    .waitForFunction(() => document.querySelector(".meters .headroom")?.textContent === "chain -", null, { timeout: 2000 })
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

  // --- effect stages: one sidebar tab each, power, a value per view,
  // sidebar summary and the passthrough badge as the engine reports it
  p = await page();
  await p.waitForSelector(".app");
  const cfgOf = (fn) => p.evaluate(`(${fn})(window.avdsp.getState().config)`);
  const badge = () => p.locator(".status .badge").textContent();
  const waitBadge = (text, why) =>
    p.waitForFunction((t) => document.querySelector(".status .badge")?.textContent?.includes(t), text, { timeout: 3000 }).catch(() => fail(`${why}: badge is "${text}"?`));
  const subOf = (name) => p.locator(".side .nav-item", { has: p.locator(".nav-name", { hasText: new RegExp(`^${name}$`) }) }).locator(".nav-sub").textContent();
  // Clear the chain: nothing on, no output headroom cut, one stream at a
  // native rate. (The app starts at 3 dB of output headroom, which counts as
  // processing, so bit-perfect needs the cut at 0 too.)
  await p.evaluate(async () => {
    const s = window.avdsp;
    s.setEqEnabled(false);
    s.setConvEnabled(false);
    s.setUpsamplingEnabled(false);
    s.setOutput({ headroom_db: 0 });
    await s.whenIdle();
  });
  await waitBadge("Bit-perfect", "an empty chain should be bit-perfect");
  const names = await p.locator(".side .nav-name").allTextContents();
  const order = ["DC block", "Pitch", "Crossfeed", "Parametric EQ", "Convolution", "Upsampling", "Harmonics", "Limiter", "Output"];
  if (JSON.stringify(names.filter((n) => order.includes(n))) !== JSON.stringify(order)) fail(`sidebar is not in chain order: ${names.join(", ")}`);
  for (const [name, on] of [
    ["DC block", (c) => c.chain.dc_block?.enabled === true],
    ["Crossfeed", (c) => c.chain.crossfeed?.enabled === true],
    ["Harmonics", (c) => c.chain.harmonics?.enabled === true],
    ["Limiter", (c) => c.chain.limiter.mode !== "off"],
  ]) {
    await p.locator(".side").getByRole("button", { name: `${name} on` }).click();
    await p.waitForFunction(`(${on})(window.avdsp.getState().config)`, null, { timeout: 3000 }).catch(() => fail(`${name}: sidebar power did not turn it on`));
    if ((await subOf(name)) === "Off") fail(`${name}: sidebar summary still says Off`);
    await waitBadge("DSP active", `${name} on should break passthrough`);
    await p.locator(".side").getByRole("button", { name: `${name} on` }).click();
    await p.waitForFunction(`!(${on})(window.avdsp.getState().config)`, null, { timeout: 3000 }).catch(() => fail(`${name}: sidebar power did not turn it off`));
    if ((await subOf(name)) !== "Off") fail(`${name}: sidebar summary should say Off`);
    await waitBadge("Bit-perfect", `${name} off should give passthrough back`);
  }
  // Pitch page: one switch for pitch shift and varispeed. On at 0 cents and 1× is on
  // (varispeed runs at 1×, so the engine reports DSP active).
  const pitchPower = p.locator(".side").getByRole("button", { name: "Pitch on", exact: true });
  const pageOn = () => p.evaluate(() => { const c = window.avdsp.getState().config; return c.chain.pitch.enabled || c.varispeed.enabled; });
  const bothAre = (on) =>
    p.waitForFunction((on) => { const c = window.avdsp.getState().config; return c.chain.pitch.enabled === on && c.varispeed.enabled === on; }, on, { timeout: 3000 });
  if ((await pageOn()) && (await pitchPower.getAttribute("aria-pressed")) !== "true") fail("the pitch page on does not show as on");
  if (await pageOn()) {
    await pitchPower.click();
    await bothAre(false).catch(() => fail("the sidebar pitch power did not turn pitch and varispeed off"));
  }
  if ((await subOf("Pitch")) !== "Off") fail(`pitch summary off: ${await subOf("Pitch")}`);
  await waitBadge("Bit-perfect", "the pitch page off should give passthrough back");
  await pitchPower.click();
  await bothAre(true).catch(() => fail("the sidebar pitch power did not turn pitch and varispeed on"));
  if ((await subOf("Pitch")) !== "On · no shift") fail(`pitch summary on at 0: ${await subOf("Pitch")}`);
  await waitBadge("DSP active", "varispeed on at 1× runs the stage");
  await p.locator(".side .nav-main", { hasText: "Pitch" }).click();
  await p.waitForSelector("#pitch-title");
  if ((await p.locator(".pt-line, .view .badge").count()) !== 0) fail("pitch view should not render a per-stage passthrough badge");
  if (((await p.locator(".view .lede").textContent()) ?? "").trim() !== "Shifts the pitch and keeps the tempo.") fail(`pitch lede: ${await p.locator(".view .lede").textContent()}`);
  if (((await p.locator(".varispeed-panel .varispeed-head + .varispeed-note").textContent()) ?? "").trim() !== "Changes speed and pitch together.") fail("varispeed description");
  if ((await p.locator(".varispeed-panel .pwr").count()) !== 0) fail("the varispeed panel still has its own power button");
  if ((await p.locator(".view .pwr").count()) !== 1) fail("the pitch page should have exactly one power button");
  await shot(p, "pitch-on-no-shift");
  await p.getByRole("button", { name: "+1 semitone" }).click();
  await p.waitForFunction(() => window.avdsp.getState().config.chain.pitch.cents === 100, null, { timeout: 3000 }).catch(() => fail("pitch +1 semitone not applied"));
  if ((await subOf("Pitch")) !== "+1 st · high") fail(`pitch summary: ${await subOf("Pitch")}`);
  await p.locator("#pitch-cents").focus();
  await p.keyboard.press("ArrowLeft");
  await p.waitForFunction(() => window.avdsp.getState().config.chain.pitch.cents === 99, null, { timeout: 3000 }).catch(() => fail("pitch arrow key did not step one cent"));
  await shot(p, "pitch-on");
  // Switching to the tuning editor keeps the 99-cent shift (no 440 → 432 on its own).
  await p.getByRole("group", { name: "How to set the shift" }).getByRole("button", { name: "Tuning (Hz)" }).click();
  await p
    .waitForFunction(() => {
      const c = window.avdsp.getState().config.chain.pitch;
      return c.reference_hz != null && Math.abs(1200 * Math.log2(c.target_hz / c.reference_hz) - 99) < 0.01 && c.enabled;
    }, null, { timeout: 3000 })
    .catch(() => fail("switching to Tuning (Hz) changed the shift"));
  await p.getByRole("group", { name: "How to set the shift" }).getByRole("button", { name: "Cents" }).click();
  await p.waitForFunction(() => window.avdsp.getState().config.chain.pitch.cents === 99, null, { timeout: 3000 }).catch(() => fail("switching back to Cents changed the shift"));
  await p.getByRole("button", { name: "Reset to 0" }).click();
  // Reaching 0 keeps the page on.
  await p.waitForFunction(() => { const c = window.avdsp.getState().config.chain.pitch; return c.cents === 0 && c.enabled; }, null, { timeout: 3000 }).catch(() => fail("pitch reset to 0 turned the stage off"));
  if ((await subOf("Pitch")) !== "On · no shift") fail(`pitch summary after reset: ${await subOf("Pitch")}`);
  // Stepping back to 0 too.
  await p.getByRole("button", { name: "+1 cent" }).click();
  await p.waitForFunction(() => window.avdsp.getState().config.chain.pitch.cents === 1, null, { timeout: 3000 }).catch(() => fail("pitch +1 cent not applied"));
  await p.getByRole("button", { name: "−1 cent" }).click();
  await p.waitForFunction(() => { const c = window.avdsp.getState().config.chain.pitch; return c.cents === 0 && c.enabled; }, null, { timeout: 3000 }).catch(() => fail("stepping to 0 turned the stage off"));
  // The view's switch turns both off, and on again.
  const viewPower = p.locator(".view-head").getByRole("button", { name: /Pitch on/ });
  await viewPower.click();
  await bothAre(false).catch(() => fail("the Pitch view's switch did not turn pitch and varispeed off"));
  if (((await p.locator(".view .hint.mono").first().textContent()) ?? "").includes("(off)")) fail("the shift readout still says (off)");
  await viewPower.click();
  await bothAre(true).catch(() => fail("the Pitch view's switch did not turn pitch and varispeed on"));
  // Varispeed's Keep pitch: the readout says "pitch held", the soften note shows beyond ±25 %.
  {
    const vp = p.locator(".varispeed-panel");
    const vs = () => p.evaluate(() => window.avdsp.getState().config.varispeed);
    await vp.getByRole("checkbox", { name: "Keep pitch" }).check();
    await p.waitForFunction(() => window.avdsp.getState().config.varispeed.keep_pitch === true, null, { timeout: 3000 }).catch(() => fail("Keep pitch did not reach the config"));
    await vp.getByRole("group", { name: "Varispeed range" }).getByRole("button", { name: "±8 %" }).waitFor();
    await p.locator("#varispeed-speed").focus();
    await p.keyboard.press("ArrowRight");
    await p.waitForFunction(() => Math.abs(window.avdsp.getState().config.varispeed.speed - 1.001) < 1e-9, null, { timeout: 3000 }).catch(() => fail("varispeed arrow key did not step 0.1 %"));
    const readout = (await vp.locator(".hint.mono").textContent()) ?? "";
    if (!readout.includes("pitch held")) fail(`Keep pitch readout: ${readout}`);
    if ((await vs()).keep_pitch !== true) fail("a speed step turned Keep pitch off");
    if ((await subOf("Pitch")) !== "Speed +0.1 %") fail(`pitch summary with a speed: ${await subOf("Pitch")}`);
    if ((await vp.locator(".varispeed-note", { hasText: "Transients soften" }).count()) !== 0) fail("soften note shown at +0.1 %");
    await vp.getByRole("group", { name: "Varispeed range" }).getByRole("button", { name: "±50 %" }).click();
    await p.evaluate(() => window.avdsp.editVarispeed({ speed: 1.3 }));
    await vp.locator(".varispeed-note", { hasText: "Transients soften" }).waitFor({ timeout: 3000 }).catch(() => fail("no soften note at +30 % with Keep pitch"));
    await shot(p, "varispeed-keep-pitch");
    await vp.getByRole("checkbox", { name: "Keep pitch" }).uncheck();
    await p.waitForFunction(() => window.avdsp.getState().config.varispeed.keep_pitch === false, null, { timeout: 3000 }).catch(() => fail("Keep pitch off did not reach the config"));
    if (!((await vp.locator(".hint.mono").textContent()) ?? "").includes(" st")) fail("semitones not back with Keep pitch off");
    if ((await vp.locator(".varispeed-note", { hasText: "Transients soften" }).count()) !== 0) fail("soften note shown with Keep pitch off");
    await vp.getByRole("button", { name: "Reset to 1×" }).click();
    await viewPower.click();
    await bothAre(false).catch(() => fail("the Pitch view's switch did not turn varispeed off"));
    await waitBadge("Bit-perfect", "the pitch page off should give passthrough back");
  }
  // DC block: arrow key on the cutoff.
  await p.locator(".side .nav-main", { hasText: "DC block" }).click();
  await p.locator(".view-head").getByRole("button", { name: "DC block on" }).click();
  await p.locator("#dc-cutoff").focus();
  await p.keyboard.press("ArrowRight");
  await p.waitForFunction(() => window.avdsp.getState().config.chain.dc_block?.cutoff_hz === 5.5, null, { timeout: 3000 }).catch(() => fail("DC block cutoff did not step to 5.5 Hz"));
  if ((await subOf("DC block")) !== "5.5 Hz high-pass") fail(`DC block summary: ${await subOf("DC block")}`);
  await shot(p, "dc-block");
  if ((await p.locator(".pt-line, .view .badge").count()) !== 0) fail("DC block view should not render a per-stage passthrough badge");
  // With playback on Direct the status bar leaves DSP active; back on DSP it reports DSP active.
  await p.evaluate(async () => {
    const st = window.avdsp;
    for (const x of st.getState().streams) await st.routeStream(x, "direct");
  });
  await p
    .waitForFunction(
      () => window.avdsp.getState().status?.active_sink === "direct" && !(document.querySelector(".status .badge")?.textContent ?? "").includes("DSP active"),
      null,
      { timeout: 3000 },
    )
    .catch(async () => fail(`status badge on Direct: ${await badge()}`));
  await p.evaluate(async () => {
    const st = window.avdsp;
    for (const x of st.getState().streams) await st.routeStream(x, "default");
  });
  await waitBadge("DSP active", "routing back to DSP should report DSP active in the status bar");
  // Crossfeed: a preset, then a custom value (the inline error path is in the store tests).
  await p.locator(".side .nav-main", { hasText: "Crossfeed" }).click();
  if ((await p.locator(".pt-line, .view .badge").count()) !== 0) fail("Crossfeed view should not render a per-stage passthrough badge");
  await p.getByRole("button", { name: "Chu Moy" }).click();
  await p.waitForFunction(() => window.avdsp.getState().config.chain.crossfeed?.mode === "bs2b_chu_moy", null, { timeout: 3000 }).catch(() => fail("crossfeed preset not applied"));
  if ((await subOf("Crossfeed")) !== "Chu Moy · 700 Hz, 6 dB") fail(`crossfeed summary: ${await subOf("Crossfeed")}`);
  // Level: always matched loudness (gain near 0 dB), with the worst-case peak shown.
  const xfFact = (k) => p.locator(".fact").filter({ has: p.locator(".k", { hasText: new RegExp(`^${k}$`) }) });
  if ((await p.getByRole("group", { name: "Crossfeed level" }).count()) !== 0) fail("crossfeed should have no level choice");
  if ((await xfFact("Level").count()) !== 1 || (await xfFact("Worst-case peak").count()) !== 1) fail("crossfeed facts missing");
  if (!(await xfFact("Worst-case peak").textContent()).includes("+")) fail(`worst-case peak: ${await xfFact("Worst-case peak").textContent()}`);
  await shot(p, "crossfeed-matched");
  await p.getByRole("button", { name: "Jan Meier" }).click();
  await p.waitForFunction(() => window.avdsp.getState().config.chain.crossfeed?.mode === "bs2b_jan_meier", null, { timeout: 3000 });
  await p.getByRole("button", { name: "Chu Moy" }).click();
  await p.waitForFunction(() => window.avdsp.getState().config.chain.crossfeed?.mode === "bs2b_chu_moy", null, { timeout: 3000 });
  await p.locator("#xf-feed").focus();
  await p.keyboard.press("ArrowRight");
  await p.waitForFunction(() => window.avdsp.getState().config.chain.crossfeed?.mode === "bs2b_custom", null, { timeout: 3000 }).catch(() => fail("moving a crossfeed slider should switch to custom"));
  // The log-scale cutoff steps one displayed step (5 Hz) per arrow key.
  await p.locator("#xf-cutoff").focus();
  await p.keyboard.press("ArrowRight");
  await p.waitForFunction(() => window.avdsp.getState().config.chain.crossfeed?.cutoff_hz === 705, null, { timeout: 3000 }).catch(async () => fail(`crossfeed cutoff arrow: ${await p.evaluate(() => window.avdsp.getState().config.chain.crossfeed?.cutoff_hz)}`));
  await p.keyboard.press("ArrowLeft");
  await p.waitForFunction(() => window.avdsp.getState().config.chain.crossfeed?.cutoff_hz === 700, null, { timeout: 3000 }).catch(() => fail("crossfeed cutoff arrow left"));
  // Mouse wheel on ValueSlider: one notch = one displayed step (even on log scale),
  // fractional touchpad deltas accumulate to one step, and the 3-condition wheel
  // gate protects page scrolling on scrollable views while allowing immediate
  // hover+wheel when the container does not scroll.
  {
    const cutoff = p.locator("#xf-cutoff");
    // 1. Non-scrollable view: unfocused hover + wheel adjusts at once.
    await cutoff.blur();
    const nonScroll = await p.evaluate(() => {
      const m = document.querySelector("#main");
      return m.scrollHeight <= m.clientHeight + 1;
    });
    if (!nonScroll) fail("expected Crossfeed at 1440x900 not to scroll");
    const cb = await cutoff.boundingBox();
    await p.mouse.move(cb.x + cb.width / 2, cb.y + cb.height / 2);
    await p.mouse.wheel(0, -100);
    await p.waitForFunction(() => window.avdsp.getState().config.chain.crossfeed?.cutoff_hz === 705, null, { timeout: 3000 }).catch(async () => fail(`crossfeed cutoff wheel up: ${await p.evaluate(() => window.avdsp.getState().config.chain.crossfeed?.cutoff_hz)}`));
    await p.waitForTimeout(450);
    if (await p.evaluate(() => window.avdsp.getState().stageDrafts.crossfeed !== undefined)) fail("wheel burst left a crossfeed draft behind");
    // Fractional touchpad deltas accumulate to one step (no premature preventDefault).
    for (let i = 0; i < 5; i++) await p.mouse.wheel(0, -20);
    await p.waitForFunction(() => window.avdsp.getState().config.chain.crossfeed?.cutoff_hz === 710, null, { timeout: 3000 }).catch(async () => fail(`crossfeed fractional touchpad wheel: ${await p.evaluate(() => window.avdsp.getState().config.chain.crossfeed?.cutoff_hz)}`));
    await p.mouse.wheel(0, 100);
    await p.mouse.wheel(0, 100);
    await p.waitForFunction(() => window.avdsp.getState().config.chain.crossfeed?.cutoff_hz === 700, null, { timeout: 3000 }).catch(async () => fail(`crossfeed wheel down: ${await p.evaluate(() => window.avdsp.getState().config.chain.crossfeed?.cutoff_hz)}`));
    await p.waitForTimeout(450);
    // Turning Crossfeed Off within the 350 ms wheel window must stay Off when the timer fires.
    await p.mouse.wheel(0, -100);
    await p.getByRole("group", { name: "Crossfeed mode" }).getByRole("button", { name: "Off" }).click();
    await p.waitForTimeout(450);
    await p.evaluate(() => window.avdsp.whenIdle());
    const afterOff = await p.evaluate(() => ({
      enabled: window.avdsp.getState().config.chain.crossfeed?.enabled,
      draft: window.avdsp.getState().stageDrafts.crossfeed,
    }));
    if (afterOff.enabled !== false || afterOff.draft !== undefined) {
      fail(`wheel timeout re-enabled Crossfeed after Off: ${JSON.stringify(afterOff)}`);
    }
    await p.evaluate(() => window.avdsp.editStage("crossfeed", (c) => ({ ...c, enabled: true, mode: "bs2b_custom", cutoff_hz: 700 })));
    await p.evaluate(() => window.avdsp.whenIdle());

    // 2. Scrollable view: unfocused slider yields to page scroll (even on a 20 px
    // delta or full notch), page scroll restarts the 300 ms hover-rest timer,
    // resting >= 300 ms with no scroll allows wheeling, and focusing allows
    // wheeling immediately.
    await p.setViewportSize({ width: 1440, height: 420 });
    await p.evaluate(() => {
      document.querySelector("#main").scrollTop = 0;
      document.activeElement?.blur?.();
    });
    const scrollInfo = await p.evaluate(() => {
      const m = document.querySelector("#main");
      return { sh: m.scrollHeight, ch: m.clientHeight };
    });
    if (!(scrollInfo.sh > scrollInfo.ch + 40)) fail(`expected scrollable Crossfeed at 1440x420: ${JSON.stringify(scrollInfo)}`);
    const sb = await cutoff.boundingBox();
    // Move pointer onto unfocused slider and immediately wheel down (20 px then 100 px):
    // page must scroll and slider value must stay at 700.
    await p.mouse.move(sb.x + sb.width / 2, sb.y + sb.height / 2);
    await p.mouse.wheel(0, 20);
    await p.mouse.wheel(0, 100);
    await p.waitForTimeout(80);
    const afterScroll1 = await p.evaluate(() => ({
      top: document.querySelector("#main").scrollTop,
      val: window.avdsp.stage("crossfeed")?.cutoff_hz,
    }));
    if (!(afterScroll1.top > 0) || afterScroll1.val !== 700) {
      fail(`unfocused slider captured initial page scroll: ${JSON.stringify(afterScroll1)}`);
    }
    // Scrolling again over the unfocused slider within 300 ms keeps scrolling the page.
    await p.mouse.wheel(0, 80);
    await p.waitForTimeout(80);
    const afterScroll2 = await p.evaluate(() => ({
      top: document.querySelector("#main").scrollTop,
      val: window.avdsp.stage("crossfeed")?.cutoff_hz,
    }));
    if (!(afterScroll2.top > afterScroll1.top) || afterScroll2.val !== 700) {
      fail(`unfocused slider interrupted ongoing page scroll: ${JSON.stringify(afterScroll2)}`);
    }
    // Condition 3: park pointer on the unfocused slider for >= 300 ms with no page scroll.
    await p.evaluate(() => { document.querySelector("#main").scrollTop = 0; });
    await p.waitForTimeout(50);
    const sb2 = await cutoff.boundingBox();
    await p.mouse.move(sb2.x + sb2.width / 2, sb2.y + sb2.height / 2);
    await p.waitForTimeout(360);
    await p.mouse.wheel(0, -100);
    await p.waitForFunction(() => window.avdsp.getState().config.chain.crossfeed?.cutoff_hz === 705, null, { timeout: 3000 }).catch(async () => fail(`hover-rest >= 300 ms did not step slider: ${await p.evaluate(() => window.avdsp.getState().config.chain.crossfeed?.cutoff_hz)}`));
    await p.waitForTimeout(450);
    // Condition 2: focused slider takes wheel immediately even right after scrolling.
    await cutoff.focus();
    await p.mouse.wheel(0, 100);
    await p.waitForFunction(() => window.avdsp.getState().config.chain.crossfeed?.cutoff_hz === 700, null, { timeout: 3000 }).catch(async () => fail(`focused slider did not step immediately: ${await p.evaluate(() => window.avdsp.getState().config.chain.crossfeed?.cutoff_hz)}`));
    await p.waitForTimeout(450);
    // Ctrl+wheel (browser zoom) is never intercepted.
    const ctrl = await p.evaluate(() => {
      const el = document.querySelector("#xf-cutoff");
      const ev = new WheelEvent("wheel", { deltaY: -100, deltaMode: 0, ctrlKey: true, bubbles: true, cancelable: true });
      el.dispatchEvent(ev);
      return { prevented: ev.defaultPrevented, val: window.avdsp.stage("crossfeed")?.cutoff_hz };
    });
    if (ctrl.prevented || ctrl.val !== 700) fail(`slider intercepted Ctrl+wheel: ${JSON.stringify(ctrl)}`);
    // Past the upper bound, wheel-up does not preventDefault so the page can scroll.
    const atMax = await p.evaluate(async () => {
      const el = document.querySelector("#xf-cutoff");
      window.avdsp.editStage("crossfeed", (c) => ({ ...c, cutoff_hz: 2000 }));
      await window.avdsp.whenIdle();
      const ev = new WheelEvent("wheel", { deltaY: -100, deltaMode: 0, bubbles: true, cancelable: true });
      el.dispatchEvent(ev);
      window.avdsp.editStage("crossfeed", (c) => ({ ...c, cutoff_hz: 700 }));
      await window.avdsp.whenIdle();
      return ev.defaultPrevented;
    });
    if (atMax) fail("slider prevented default when scrolling past its maximum");
    await p.setViewportSize({ width: 1440, height: 900 });
  }
  await shot(p, "crossfeed");
  {
    // Light theme: the crossfeed level choice and the Gaussian window in Custom.
    const pl = await page("", "light");
    await pl.waitForSelector(".app");
    await pl.locator(".side .nav-main", { hasText: "Crossfeed" }).click();
    await pl.getByRole("button", { name: "Default", exact: true }).click();
    await pl.waitForFunction(() => window.avdsp.getState().config.chain.crossfeed?.enabled === true, null, { timeout: 3000 }).catch(() => fail("crossfeed on (light)"));
    await shot(pl, "crossfeed-light");
    await pl.evaluate(() => localStorage.setItem("avdsp-upsampling-advanced", "1"));
    await pl.evaluate(() => window.avdsp.setUpsampler({ target: { ratio: 4 }, design: { ...window.avdsp.getState().config.chain.upsampler.design, window: { type: "gaussian", stopband_db: 150, sampling: "symmetric" } } }));
    await pl.locator(".side .nav-main", { hasText: "Upsampling" }).click();
    const stop = pl.locator(".field", { has: pl.locator(".lbl", { hasText: /^Stopband$/ }) });
    await stop.waitFor({ timeout: 3000 }).catch(() => fail("Gaussian stopband field (light)"));
    await stop.scrollIntoViewIfNeeded();
    await shot(pl, "upsampling-gaussian-light");
    await pl.close();
  }
  // Harmonics: pentode, THD shown, and all harmonic slider rows line up.
  await p.locator(".side .nav-main", { hasText: "Harmonics" }).click();
  if ((await p.locator(".pt-line, .view .badge").count()) !== 0) fail("Harmonics view should not render a per-stage passthrough badge");
  {
    const align = await p.evaluate(() => {
      const seg = document.querySelector(".view .seg")?.getBoundingClientRect();
      const sliders = [...document.querySelectorAll('.view .vslider input[type="range"]')].map((e) => e.getBoundingClientRect());
      const outs = [...document.querySelectorAll(".view .vslider output")].map((e) => e.getBoundingClientRect());
      return {
        segLeft: seg?.left ?? -1,
        lefts: sliders.map((r) => r.left),
        rights: sliders.map((r) => r.right),
        outLefts: outs.map((r) => r.left),
      };
    });
    if (align.lefts.length !== 4) fail(`expected 4 Harmonics sliders, got ${align.lefts.length}`);
    else {
      const same = (xs) => xs.every((x) => Math.abs(x - xs[0]) < 0.5);
      if (!same(align.lefts) || !same(align.rights)) fail(`Harmonics sliders are misaligned: lefts=${JSON.stringify(align.lefts)} rights=${JSON.stringify(align.rights)}`);
      if (!same(align.outLefts)) fail(`Harmonics readouts are misaligned: ${JSON.stringify(align.outLefts)}`);
      if (Math.abs(align.lefts[0] - align.segLeft) > 1) fail(`Harmonics sliders (${align.lefts[0]}) do not align with Character (${align.segLeft})`);
    }
  }
  await p.getByRole("button", { name: "Pentode" }).click();
  await p.waitForFunction(() => window.avdsp.getState().config.chain.harmonics?.character === "pentode", null, { timeout: 3000 }).catch(() => fail("harmonics pentode not applied"));
  if (!(await subOf("Harmonics")).startsWith("Pentode · THD")) fail(`harmonics summary: ${await subOf("Harmonics")}`);
  await shot(p, "harmonics");
  // Limiter: limit mode, the reduction meter and the strip readout.
  await p.locator(".side .nav-main", { hasText: "Limiter" }).click();
  if ((await p.locator(".pt-line, .view .badge").count()) !== 0) fail("Limiter view should not render a per-stage passthrough badge");
  await p.locator(".seg").getByRole("button", { name: "Limit", exact: true }).click();
  await p.waitForFunction(() => window.avdsp.getState().config.chain.limiter.mode === "limit", null, { timeout: 3000 }).catch(() => fail("limiter mode not applied"));
  await p.waitForSelector(".meters .gr-mini:not([hidden])", { timeout: 3000 }).catch(() => fail("gain reduction missing from the meter strip"));
  await p
    .waitForFunction(() => /\d/.test(document.querySelector(".gr .fact .v")?.textContent ?? ""), null, { timeout: 3000 })
    .catch(() => fail("limiter meter shows no reading"));
  await p
    .waitForFunction(
      () => /^(Playing, below the ceiling: no reduction|Reducing)$/.test(document.querySelector(".gr-head .hint")?.textContent ?? ""),
      null,
      { timeout: 3000 },
    )
    .catch(async () => fail(`limiter playing state text: ${await p.locator(".gr-head .hint").textContent()}`));
  // While playing with signal below the ceiling (muted stream keeps playback active at −120 dBFS),
  // the meter reports playing below the ceiling rather than idle.
  await p.getByRole("button", { name: "Mute", exact: true }).click();
  await p
    .waitForFunction(() => document.querySelector(".gr-head .hint")?.textContent === "Playing, below the ceiling: no reduction", null, { timeout: 3000 })
    .catch(async () => fail(`limiter below-ceiling text: ${await p.locator(".gr-head .hint").textContent()}`));
  // Bypass takes the limiter out of the path.
  await p.getByRole("button", { name: "Bypass" }).click();
  await p
    .waitForFunction(() => document.querySelector(".gr-head .hint")?.textContent === "Not in the path (Direct, bypass or nothing playing)", null, { timeout: 3000 })
    .catch(async () => fail(`limiter bypass text: ${await p.locator(".gr-head .hint").textContent()}`));
  await p.getByRole("button", { name: "Bypass" }).click();
  await p
    .waitForFunction(() => document.querySelector(".gr-head .hint")?.textContent === "Playing, below the ceiling: no reduction", null, { timeout: 3000 })
    .catch(async () => fail(`limiter text after un-bypass: ${await p.locator(".gr-head .hint").textContent()}`));
  await p.getByRole("button", { name: "Mute", exact: true }).click();
  if ((await p.locator(".gr .fact .k").last().textContent()).toLowerCase() !== "onsets") fail("limit mode should count onsets");
  await p.locator("#lim-ceiling").focus();
  await p.keyboard.press("ArrowLeft");
  await p.waitForFunction(() => window.avdsp.getState().config.chain.limiter.ceiling_dbfs === -0.6, null, { timeout: 3000 }).catch(() => fail("limiter ceiling did not step"));
  await p.locator(".seg").getByRole("button", { name: "Auto headroom" }).click();
  await p.waitForFunction(() => document.querySelector(".gr .fact:last-child .k")?.textContent === "headroom steps", null, { timeout: 3000 }).catch(() => fail("auto mode should count headroom steps"));
  await shot(p, "limiter");
  await p.locator(".seg").getByRole("button", { name: "Off", exact: true }).click();
  await p.waitForFunction(() => window.avdsp.getState().config.chain.limiter.mode === "off", null, { timeout: 3000 }).catch(() => fail("limiter Off not applied"));
  // Output headroom lives here and stays editable with the limiter off.
  if (await p.getByLabel("Headroom", { exact: true }).isDisabled()) fail("headroom field disabled with the limiter off");
  await p.getByLabel("Headroom", { exact: true }).fill("4");
  await p.getByLabel("Headroom", { exact: true }).press("Enter");
  await p.waitForFunction(() => window.avdsp.getState().config.chain.output.headroom_db === 4, null, { timeout: 3000 }).catch(() => fail("headroom 4 dB not sent"));
  if ((await subOf("Limiter")) !== "Off · −4 dB") fail(`limiter summary with headroom: ${await subOf("Limiter")}`);
  await p.locator("#out-headroom").focus();
  await p.keyboard.press("ArrowLeft");
  await p.waitForFunction(() => window.avdsp.getState().config.chain.output.headroom_db === 3.5, null, { timeout: 3000 }).catch(() => fail("headroom slider did not step to 3.5 dB"));
  if ((await p.getByLabel("Headroom", { exact: true }).inputValue()) !== "3.5") fail("headroom field does not follow the slider");
  await p.evaluate(async () => {
    window.avdsp.setOutput({ headroom_db: 0 });
    await window.avdsp.whenIdle();
  });
  await p.locator(".side").getByRole("button", { name: "Limiter on" }).click();
  await p.waitForFunction(() => window.avdsp.getState().config.chain.limiter.mode === "auto_headroom", null, { timeout: 3000 }).catch(async () => fail(`sidebar power after Off: ${await p.evaluate(() => window.avdsp.getState().config.chain.limiter.mode)}`));
  // EQ tilt: on, in the summary and the engine's status.
  await p.evaluate(() => window.avdsp.setEqEnabled(true));
  await p.locator(".side .nav-main", { hasText: "Parametric EQ" }).click();
  await p.locator(".tilt-panel").getByRole("button", { name: "Tilt on" }).click();
  await p.waitForFunction(() => window.avdsp.getState().status?.eq_tilt !== null, null, { timeout: 3000 }).catch(() => fail("tilt did not reach the engine"));
  if (!(await subOf("Parametric EQ")).includes("tilt")) fail(`EQ summary misses the tilt: ${await subOf("Parametric EQ")}`);
  await shot(p, "eq-tilt");
  // Tilt shapes: whole band, above and below a frequency.
  const tiltOf = () => p.evaluate(() => window.avdsp.getState().config.chain.eq.tilt);
  const tiltShape = (name) => p.locator(".tilt-panel").getByRole("group", { name: "Tilt shape" }).getByRole("button", { name, exact: true });
  const tiltIs = (want, what) =>
    p
      .waitForFunction((w) => Object.entries(w).every(([k, v]) => JSON.stringify(window.avdsp.getState().config.chain.eq.tilt[k]) === JSON.stringify(v)), want, { timeout: 3000 })
      .catch(async () => fail(`${what}: ${JSON.stringify(await tiltOf())}`));
  await tiltIs({ start_hz: 20, end_hz: 20000 }, "tilt whole band");
  if ((await tiltShape("Whole band").getAttribute("aria-pressed")) !== "true") fail("a new tilt should read as Whole band");
  await tiltShape("Above").click();
  await tiltIs({ start_hz: 1000, end_hz: 20000, pivot: "start" }, "tilt above 1 kHz");
  if (!(await subOf("Parametric EQ")).includes("tilt −1.0 dB/oct above 1 kHz")) fail(`summary for tilt above: ${await subOf("Parametric EQ")}`);
  await p.locator("#tilt-corner").focus();
  await p.keyboard.press("End");
  await tiltIs({ start_hz: 17679, end_hz: 20000 }, "tilt corner at the top stays 1/6 oct below the end");
  await tiltShape("Below").click();
  await tiltIs({ start_hz: 20, end_hz: 17679, pivot: "end" }, "tilt below keeps the corner");
  await p.evaluate(async () => {
    window.avdsp.editEq((eq) => ({ ...eq, tilt: { ...eq.tilt, end_hz: 200 } }));
    await window.avdsp.whenIdle();
  });
  if (!(await subOf("Parametric EQ")).includes("below 200 Hz")) fail(`summary for tilt below: ${await subOf("Parametric EQ")}`);
  if ((await tiltShape("Below").getAttribute("aria-pressed")) !== "true") fail("tilt below 200 Hz should read as Below");
  await tiltShape("Whole band").click();
  await tiltIs({ start_hz: 20, end_hz: 20000, pivot: "center" }, "tilt back to whole band");
  // A band set elsewhere reads as the closest shape and is not rewritten.
  await p.evaluate(async () => {
    window.avdsp.editEq((eq) => ({ ...eq, tilt: { ...eq.tilt, start_hz: 300, end_hz: 16000, pivot: "center" } }));
    await window.avdsp.whenIdle();
  });
  if ((await tiltShape("Above").getAttribute("aria-pressed")) !== "true") fail("a 300 Hz–16 kHz tilt should read as Above");
  await tiltIs({ start_hz: 300, end_hz: 16000, pivot: "center" }, "an outside band was rewritten");
  await tiltShape("Whole band").click();
  await tiltIs({ start_hz: 20, end_hz: 20000 }, "tilt whole band again");
  // Both: a bass group and a treble group side by side, 0 dB between.
  const highOf = () => p.evaluate(() => window.avdsp.getState().config.chain.eq.tilt_high ?? null);
  const highIs = (want, what) =>
    p
      .waitForFunction((w) => Object.entries(w).every(([k, v]) => JSON.stringify(window.avdsp.getState().config.chain.eq.tilt_high?.[k]) === JSON.stringify(v)), want, { timeout: 3000 })
      .catch(async () => fail(`${what}: ${JSON.stringify(await highOf())}`));
  await tiltShape("Both").click();
  await tiltIs({ start_hz: 20, end_hz: 200, pivot: "end" }, "tilt both: bass section");
  await highIs({ start_hz: 2000, end_hz: 20000, pivot: "start" }, "tilt both: treble section");
  if ((await tiltShape("Both").getAttribute("aria-pressed")) !== "true") fail("two sections should read as Both");
  if (!(await subOf("Parametric EQ")).includes("tilt −1.0 dB/oct below 200 Hz, −1.0 dB/oct above 2 kHz")) fail(`summary for tilt both: ${await subOf("Parametric EQ")}`);
  const sides = await p.locator(".tilt-panel .tilt-side").evaluateAll((els) => els.map((e) => e.getBoundingClientRect().top));
  if (sides.length !== 2 || Math.abs(sides[0] - sides[1]) > 4) fail(`tilt both: groups not side by side: ${JSON.stringify(sides)}`);
  // The bass corner stops below the treble one; the treble slope is its own.
  await p.locator("#tilt-corner").focus();
  await p.keyboard.press("End");
  await tiltIs({ end_hz: 1999 }, "tilt both: bass corner stops below the treble corner");
  await p.locator("#tilt-high-slope").focus();
  await p.keyboard.press("Home");
  await highIs({ slope_db_per_oct: -6 }, "tilt both: treble slope");
  await tiltIs({ slope_db_per_oct: -1 }, "tilt both: bass slope kept");
  await p.waitForFunction(() => window.avdsp.getState().status?.eq_tilt?.effective_start_hz === 20 && !window.avdsp.getState().status?.eq_tilt?.error, null, { timeout: 3000 }).catch(async () => fail(`tilt both status: ${JSON.stringify(await p.evaluate(() => window.avdsp.getState().status?.eq_tilt))}`));
  await p.locator(".tilt-panel").screenshot({ path: resolve(out, "tilt-panel-both.png") });
  await tiltShape("Whole band").click();
  await tiltIs({ start_hz: 20, end_hz: 20000 }, "tilt whole band from both");
  if ((await highOf()) !== null) fail("leaving Both should drop the treble section");
  // The tilt panel in Graphite and Light, set to Above.
  for (const skin of ["graphite", "light"]) {
    const q = await page("", skin);
    await q.evaluate(() => window.avdsp.setEqEnabled(true));
    await q.locator(".side .nav-main", { hasText: "Parametric EQ" }).click();
    await q.locator(".tilt-panel").getByRole("button", { name: "Tilt on" }).click();
    await q.locator(".tilt-panel").getByRole("group", { name: "Tilt shape" }).getByRole("button", { name: "Above" }).click();
    await q.waitForFunction(() => window.avdsp.getState().config.chain.eq.tilt.pivot === "start", null, { timeout: 3000 }).catch(() => fail(`${skin}: tilt Above not applied`));
    await q.locator(".tilt-panel").screenshot({ path: resolve(out, `tilt-panel-${skin}.png`) });
    await q.locator(".tilt-panel").getByRole("group", { name: "Tilt shape" }).getByRole("button", { name: "Whole band" }).click();
    await q.locator(".tilt-panel").screenshot({ path: resolve(out, `tilt-panel-whole-${skin}.png`) });
    await q.context().close();
  }
  // Output: a noise shaper.
  await p.locator(".side .nav-main", { hasText: "Output" }).click();
  await p.getByRole("group", { name: "Noise shaping" }).getByRole("button", { name: "Broad" }).click();
  await p.waitForFunction(() => window.avdsp.getState().config.chain.output.dither === "feedback5", null, { timeout: 3000 }).catch(() => fail("noise shaper not applied"));
  if (!(await subOf("Output")).includes("broad shaping")) fail(`output summary: ${await subOf("Output")}`);
  if (/dB/.test(await subOf("Output"))) fail(`output summary still shows the headroom: ${await subOf("Output")}`);
  if ((await p.locator(".view").getByLabel("Headroom", { exact: true }).count()) !== 0) fail("headroom still on the Output page");
  await shot(p, "output-dither");
  // Device: volume guard.
  await p.locator(".side .nav-main", { hasText: "Device" }).click();
  await p.getByRole("group", { name: "Volume guard" }).getByRole("button", { name: "Lock" }).click();
  await p.waitForFunction(() => window.avdsp.getState().status?.volume?.guard === "lock", null, { timeout: 3000 }).catch(() => fail("volume guard not applied"));
  await shot(p, "device-volume");
  // Limit: "No rise limit" sends 0 and disables the rate; off restores the last rate.
  await p.getByRole("group", { name: "Volume guard" }).getByRole("button", { name: "Limit" }).click();
  await p.waitForFunction(() => window.avdsp.getState().config.volume.guard.mode === "limit", null, { timeout: 3000 }).catch(() => fail("volume guard Limit not applied"));
  const riseBefore = await p.evaluate(() => window.avdsp.getState().config.volume.guard.max_rise_db_per_s);
  const noRise = p.getByRole("checkbox", { name: "No rise limit (only the cap)" });
  if (await noRise.isChecked()) fail("No rise limit is on with a non-zero rate");
  await noRise.click();
  await p.waitForFunction(() => window.avdsp.getState().config.volume.guard.max_rise_db_per_s === 0, null, { timeout: 3000 }).catch(() => fail("No rise limit did not send 0"));
  if (!(await p.locator("#vol-rise").isDisabled())) fail("the rise rate stays editable with no rise limit");
  await shot(p, "device-volume-no-rise-limit");
  await noRise.click();
  await p.waitForFunction((r) => window.avdsp.getState().config.volume.guard.max_rise_db_per_s === r, riseBefore, { timeout: 3000 }).catch(() => fail(`turning No rise limit off did not restore ${riseBefore} dB/s`));
  void cfgOf;
  void badge;
  await p.close();

  // --- Routing: per-app stream volume (slider, mute, cap note, refusals)
  p = await page();
  await p.waitForSelector(".app");
  await p.locator(".nav-main", { hasText: "Routing" }).click();
  {
    const streamDb = (node) => p.evaluate((n) => window.avdsp.getState().streams.find((s) => s.node === n)?.volume_db, node);
    const slider = p.getByRole("slider", { name: "Volume of Strawberry" });
    await slider.waitFor({ timeout: 3000 });
    if ((await slider.getAttribute("aria-valuetext")) !== "0.0 dB") fail(`stream slider does not read 0.0 dB: ${await slider.getAttribute("aria-valuetext")}`);
    // Keyboard: Page Down twice = -12 dB, sent with set_stream_volume.
    await slider.focus();
    await p.keyboard.press("PageDown");
    await p.keyboard.press("PageDown");
    await p.waitForFunction(() => window.avdsp.getState().streams.find((s) => s.node === 81)?.volume_db === -12, null, { timeout: 3000 }).catch(() => fail("Page Down twice did not set the stream to -12 dB"));
    // Dragging to the left end then back up: drafts while moving, the answer at the end.
    const b = await slider.boundingBox();
    await p.mouse.move(b.x + b.width * 0.9, b.y + b.height / 2);
    await p.mouse.down();
    await p.mouse.move(b.x + b.width * 0.6, b.y + b.height / 2, { steps: 8 });
    await p.mouse.move(b.x + b.width * 0.95, b.y + b.height / 2, { steps: 8 });
    await p.mouse.up();
    await p.waitForTimeout(300);
    const dragged = await streamDb(81);
    const shown = Number(((await slider.getAttribute("aria-valuenow")) ?? "NaN"));
    if (!(dragged > -12 && Math.abs(dragged - shown) < 0.05)) fail(`stream drag did not land where shown: engine ${dragged}, slider ${shown}`);
    if (await p.evaluate(() => Object.keys(window.avdsp.getState().streamDrafts).length)) fail("a stream draft stayed after the drag ended");
    // Mute toggles and keeps the level.
    await p.getByRole("button", { name: "Mute Strawberry" }).click();
    await p.waitForFunction(() => window.avdsp.getState().streams.find((s) => s.node === 81)?.muted === true, null, { timeout: 3000 }).catch(() => fail("stream mute did not apply"));
    if ((await p.getByRole("button", { name: "Mute Strawberry" }).getAttribute("aria-pressed")) !== "true") fail("stream mute button not pressed");
    if ((await streamDb(81)) !== dragged) fail("muting changed the stream level");
    await p.getByRole("button", { name: "Mute Strawberry" }).click();
    await p.waitForFunction(() => window.avdsp.getState().streams.find((s) => s.node === 81)?.muted === false, null, { timeout: 3000 }).catch(() => fail("stream unmute did not apply"));
    // The app stream cap holds the level, and the note says why beside the slider.
    await p.evaluate(() => window.avdsp.setVolumeSettings((v) => ({ ...v, guard: { ...v.guard, stream_max_db: -20 } })));
    await p.waitForFunction(() => window.avdsp.getState().config.volume.guard.stream_max_db === -20, null, { timeout: 3000 }).catch(() => fail("could not set the stream cap"));
    await slider.focus();
    await p.keyboard.press("ArrowUp");
    await p.waitForSelector(".stream-vol-note", { timeout: 3000 }).catch(() => fail("no note beside a capped stream"));
    if ((await streamDb(81)) !== -20) fail(`capped stream is at ${await streamDb(81)} dB, not -20`);
    await shot(p, "routing-stream-volume");
    // FineSlider in a scrollable view obeys the same wheel gate: unfocused
    // immediate wheel scrolls the page without moving the slider, and focused
    // wheel adjusts the slider.
    await p.evaluate(() => window.avdsp.setVolumeSettings((v) => ({ ...v, guard: { ...v.guard, stream_max_db: null } })));
    await p.waitForFunction(() => window.avdsp.getState().config.volume.guard.stream_max_db === null, null, { timeout: 3000 }).catch(() => fail("could not clear the stream cap"));
    {
      await p.setViewportSize({ width: 1440, height: 340 });
      await p.evaluate(() => {
        document.querySelector("#main").scrollTop = 0;
        document.activeElement?.blur?.();
      });
      const fb = await slider.boundingBox();
      const beforeFine = await streamDb(81);
      await p.mouse.move(fb.x + fb.width / 2, fb.y + fb.height / 2);
      await p.mouse.wheel(0, 20);
      await p.mouse.wheel(0, 100);
      await p.waitForTimeout(80);
      const afterFineScroll = await p.evaluate(() => document.querySelector("#main").scrollTop);
      if (!(afterFineScroll > 0) || (await streamDb(81)) !== beforeFine) {
        fail(`unfocused FineSlider captured page scroll: scrollTop=${afterFineScroll}, db=${await streamDb(81)} vs ${beforeFine}`);
      }
      await p.setViewportSize({ width: 1440, height: 900 });
    }
    // Leaving Routing mid-gesture (a wheel step, inside its 350 ms pause)
    // still ends it: the draft clears and the engine has the value.
    {
      const wb = await slider.boundingBox();
      const beforeWheel = await streamDb(81);
      await slider.focus();
      await p.mouse.move(wb.x + wb.width / 2, wb.y + wb.height / 2);
      await p.mouse.wheel(0, -100);
      await p.locator(".nav-main", { hasText: "Device" }).click();
      await p.waitForTimeout(600);
      if (await p.evaluate(() => Object.keys(window.avdsp.getState().streamDrafts).length)) fail("leaving Routing mid-wheel left a stream draft behind");
      const afterWheel = await streamDb(81);
      if (!(afterWheel > beforeWheel)) fail(`the wheel step before leaving Routing was not sent: ${beforeWheel} -> ${afterWheel}`);
      await p.locator(".nav-main", { hasText: "Routing" }).click();
      await slider.waitFor({ timeout: 3000 });
    }
    // A stream not on our sinks is shown but cannot be set.
    const firefox = p.getByRole("slider", { name: "Volume of Firefox" });
    if ((await firefox.getAttribute("aria-disabled")) !== "true") fail("a stream not playing through Audivane has an enabled slider");
    if (!((await firefox.getAttribute("title")) ?? "").includes("not playing through Audivane")) fail("the disabled stream slider does not say why");
  }
  await p.close();

  // --- Routing on an engine without set_stream_volume: disabled, says to update
  // The same in the light theme, for the screenshot.
  p = await page("", "light");
  await p.waitForSelector(".app");
  await p.locator(".nav-main", { hasText: "Routing" }).click();
  await p.evaluate(() => window.avdsp.setVolumeSettings((v) => ({ ...v, guard: { ...v.guard, stream_max_db: -20 } })));
  await p.waitForFunction(() => window.avdsp.getState().config.volume.guard.stream_max_db === -20, null, { timeout: 3000 }).catch(() => fail("could not set the stream cap (light)"));
  await p.getByRole("slider", { name: "Volume of Strawberry" }).focus();
  await p.keyboard.press("ArrowDown");
  await p.evaluate(() => window.avdsp.setStreamLevel(94, { volume_db: -6 }));
  await p.waitForSelector(".stream-vol-cell .inline-err", { timeout: 3000 }).catch(() => fail("no inline error beside a refused stream"));
  await shot(p, "routing-stream-volume-light");
  await p.close();

  p = await page("?legacy_streams", "midnight");
  await p.waitForSelector(".app");
  await p.locator(".nav-main", { hasText: "Routing" }).click();
  {
    const slider = p.getByRole("slider", { name: "Volume of Strawberry" });
    await slider.waitFor({ timeout: 3000 });
    if ((await slider.getAttribute("aria-disabled")) !== "true") fail("stream slider enabled on an engine without set_stream_volume");
    if (!((await slider.getAttribute("title")) ?? "").includes("Update the engine")) fail("the legacy stream slider does not say to update the engine");
    if (!(await p.getByRole("button", { name: "Mute Strawberry" }).isDisabled())) fail("stream mute enabled on an engine without set_stream_volume");
    await shot(p, "routing-stream-volume-legacy");
  }
  await p.close();

  // --- engine down, then start
  p = await page("?engine=down", "graphite", [1000, 700]);
  await p.waitForSelector(".down-card");
  await shot(p, "engine-down");
  await p.getByRole("button", { name: "Start engine" }).click();
  await p.waitForSelector(".app", { timeout: 5000 });
  await p.close();

  // --- device self-test, light theme, narrow + status bar responsiveness (1000, 1280, 1440 px)
  p = await page("", "light", [1000, 700]);
  await p.waitForSelector(".app");
  // Check playing state with limiter off and on across 1000, 1280 and 1440 px:
  // now-playing stays visible with a readable floor and ellipsizes, never hidden
  // by the limiter's gain reduction, and the status bar stays single-line without overflow.
  {
    const checkBar = async (w, limiterOn) => {
      await p.setViewportSize({ width: w, height: 700 });
      await p.evaluate((on) => window.avdsp.setStageEnabled("limiter", on), limiterOn);
      await p.evaluate(() => window.avdsp.whenIdle());
      if (limiterOn) {
        await p.waitForSelector(".meters .gr-mini", { state: "visible", timeout: 3000 }).catch(() => fail(`gain reduction hidden at ${w} px`));
      }
      const r = await p.locator(".status").evaluate((el) => {
        const kids = [...el.children].filter((c) => getComputedStyle(c).display !== "none");
        const rows = new Set(kids.map((c) => Math.round(c.getBoundingClientRect().top / 10))).size;
        const np = el.querySelector(".np");
        const npStyle = np ? getComputedStyle(np) : null;
        const npBox = np ? np.getBoundingClientRect() : null;
        return {
          rows,
          overflow: el.scrollWidth - el.clientWidth,
          npVisible: !!npStyle && npStyle.display !== "none" && !!npBox && npBox.width >= 90 && (np.textContent?.trim().length ?? 0) > 0,
          npWidth: npBox ? Math.round(npBox.width) : 0,
          npEllipsis: npStyle?.textOverflow === "ellipsis",
        };
      });
      const tag = `${w} px (limiter ${limiterOn ? "on" : "off"})`;
      if (r.rows !== 1) fail(`status bar wraps at ${tag}: ${r.rows} rows`);
      if (r.overflow > 1) fail(`status bar overflows horizontally at ${tag}: +${r.overflow} px`);
      if (!r.npVisible || !r.npEllipsis) fail(`now-playing missing or too narrow at ${tag}: ${JSON.stringify(r)}`);
    };
    for (const w of [1440, 1280, 1000]) {
      await checkBar(w, false);
      await checkBar(w, true);
      if (w === 1440) {
        // Changing digit counts in DSP load, xruns (including 9999 -> 10000 -> 99999+)
        // and latency must not shift later readouts.
        const jitter = await p.evaluate(() => {
          const load = document.querySelector(".status .stat-load");
          const xruns = document.querySelector(".status .stat-xruns");
          const lat = document.querySelector(".status .stat-latency");
          const stats = document.querySelector(".status .stats");
          const snap = () => ({
            xrunsLeft: xruns.getBoundingClientRect().left,
            latLeft: lat.getBoundingClientRect().left,
            statsRight: stats.getBoundingClientRect().right,
          });
          const orig = [load.textContent, xruns.textContent, lat.textContent];
          load.textContent = "DSP 8%";
          xruns.textContent = "0 xruns";
          lat.textContent = "4.2 ms";
          const a = snap();
          xruns.textContent = "9999 xruns";
          const b9999 = snap();
          xruns.textContent = "10000 xruns";
          const b10000 = snap();
          load.textContent = "DSP 100%";
          xruns.textContent = "99999+ xruns";
          lat.textContent = "999.9 ms";
          const b = snap();
          [load.textContent, xruns.textContent, lat.textContent] = orig;
          return {
            dxXruns: Math.abs(b.xrunsLeft - a.xrunsLeft),
            dxLat: Math.abs(b.latLeft - a.latLeft),
            dxLat5: Math.abs(b10000.latLeft - b9999.latLeft),
            dxRight: Math.abs(b.statsRight - a.statsRight),
          };
        });
        if (jitter.dxXruns > 0.5 || jitter.dxLat > 0.5 || jitter.dxLat5 > 0.5 || jitter.dxRight > 0.5) {
          fail(`status bar stats jitter when digit counts change: ${JSON.stringify(jitter)}`);
        }
      }
      if (w === 1000) {
        // Narrow-hidden readouts stay in the accessibility tree (visually hidden,
        // never display: none) so screen readers keep latency, channels, peak and chain gain.
        const a11y = await p.locator(".status").evaluate((el) =>
          [".path .ch", ".meters .headroom", ".meters .readout", ".stats .stat-latency"].map((sel) => {
            const node = el.querySelector(sel);
            const st = node ? getComputedStyle(node) : null;
            return { sel, exists: !!node, display: st?.display, text: node?.textContent?.trim() ?? "" };
          }),
        );
        for (const item of a11y) {
          if (!item.exists || item.display === "none" || !item.text) {
            fail(`narrow status readout removed from accessibility tree: ${JSON.stringify(item)}`);
          }
        }
      }
    }
    // Wide rate path (44.1 -> 705.6 kHz, ×16) + limiter on + engine error
    // + open output-check dialog across 900, 1000, 1280 and 1440 px:
    // zero horizontal overflow, readable error box, and dialog inside the viewport.
    // The path and the error live in the mock, so the status it keeps pushing
    // during the check (limiter headroom, meters) never drops them.
    const upsBefore = await p.evaluate(() => window.avdsp.getState().config.chain.upsampler.target);
    await p.evaluate(async () => {
      const s = window.avdsp;
      s.setUpsampler({ target: { ratio: 16 } });
      await s.whenIdle();
      const daemon = window.avdspShell.daemon;
      daemon.setXruns(100000);
      daemon.setEngineError("ALSA output error: device busy (EBUSY)");
    });
    await p.waitForSelector(".status .status-err", { timeout: 3000 }).catch(() => fail("status-err did not render"));
    if ((await p.locator(".status .stat-xruns").textContent())?.trim() !== "99999+ xruns") {
      fail(`xrun cap did not render 99999+ xruns: ${await p.locator(".status .stat-xruns").textContent()}`);
    }
    const badgeBtn = p.locator(".status .badge");
    await badgeBtn.click();
    await p.waitForSelector(".status .popover", { timeout: 3000 }).catch(() => fail("output-check popover did not open"));
    for (const w of [1440, 1280, 1000, 900]) {
      await p.setViewportSize({ width: w, height: 700 });
      await p.waitForTimeout(80);
      const r = await p.locator(".status").evaluate((el) => {
        const err = el.querySelector(".status-err")?.getBoundingClientRect();
        const np = el.querySelector(".np")?.getBoundingClientRect();
        const pop = el.querySelector(".popover")?.getBoundingClientRect();
        return {
          overflow: el.scrollWidth - el.clientWidth,
          errW: err ? Math.round(err.width) : 0,
          errH: err ? Math.round(err.height) : 0,
          npW: np ? Math.round(np.width) : 0,
          popLeft: pop ? Math.round(pop.left) : -1,
          popRight: pop ? Math.round(pop.right) : 99999,
          vw: window.innerWidth,
        };
      });
      if (r.overflow > 1) fail(`wide+error status bar overflows at ${w} px: ${JSON.stringify(r)}`);
      if (r.errW < 180 || r.errH < 12) fail(`engine error collapsed at ${w} px: ${JSON.stringify(r)}`);
      if (r.npW < 90) fail(`now-playing collapsed on wide+error bar at ${w} px: ${JSON.stringify(r)}`);
      if (r.popLeft < 0 || r.popRight > r.vw) fail(`output-check popover outside viewport at ${w} px: ${JSON.stringify(r)}`);
    }
    await badgeBtn.click();
    await p.evaluate(async (target) => {
      const s = window.avdsp;
      const daemon = window.avdspShell.daemon;
      daemon.setXruns(0);
      daemon.setEngineError(null);
      s.setUpsampler({ target });
      await s.whenIdle();
    }, upsBefore);
    await p.setViewportSize({ width: 1000, height: 700 });
  }
  await shot(p, "status-narrow-limiter");
  await p.getByRole("button", { name: /^Device/ }).click();
  await p.getByRole("button", { name: "Run bit-perfect self-test" }).click();
  await p.waitForSelector(".result:not(.pending)", { timeout: 5000 });
  await shot(p, "device-selftest");
  await p.close();

  // --- tray popup (its own window: ?window=popup), keyboard and mouse
  for (const skin of ["midnight", "light"]) {
    const ctx = await browser.newContext({ viewport: { width: 320, height: 184 } });
    await ctx.addInitScript((s) => localStorage.setItem("avdsp-skin", s), skin);
    p = await ctx.newPage();
    p.on("pageerror", (e) => fail(`popup pageerror: ${e.message}`));
    p.on("console", (m) => m.type() === "error" && fail(`popup console: ${m.text()}`));
    await p.goto(url + "?window=popup");
    await p.waitForSelector(".popup .fine-slider:not([aria-disabled])", { timeout: 4000 });
    const vol = () => p.evaluate(() => window.avdsp.getState().status.volume);
    const v0 = await vol();
    // The slider has the focus when the popup opens: arrows work at once.
    if ((await p.evaluate(() => document.activeElement?.id)) !== "popup-vol") fail("popup: slider not focused on open");
    await p.keyboard.press("ArrowUp");
    await p.waitForFunction((v) => window.avdsp.getState().status.volume.volume_db === v + 0.5, v0.volume_db, { timeout: 3000 }).catch(() => fail("popup: ArrowUp did not raise the volume by 0.5 dB"));
    const readout = await p.locator(".popup-level output").innerText();
    if (!readout.includes("dB")) fail(`popup readout "${readout}"`);
    await p.getByRole("button", { name: "Mute" }).click();
    await p.waitForFunction(() => window.avdsp.getState().status.volume.muted === true, null, { timeout: 3000 }).catch(() => fail("popup: mute did not mute"));
    if ((await vol()).volume_db !== v0.volume_db + 0.5) fail("popup: mute changed the volume");
    await shot(p, `popup-${skin}`);
    await p.getByRole("button", { name: "Mute" }).click();
    await p.locator("#popup-vol").focus();
    await p.keyboard.press("Escape");
    if (!(await p.evaluate(() => window.avdspShell.shellCalls.includes("hide-popup")))) fail("popup: Escape did not close it");
    await p.getByRole("button", { name: "Open Audivane DSP" }).click();
    if (!(await p.evaluate(() => window.avdspShell.shellCalls.includes("show-main")))) fail("popup: Open Audivane DSP did not show the main window");
    await ctx.close();
  }

  // --- on-screen display (?window=osd): matches the popup's level row, bar fill and muted state
  {
    const popCtx = await browser.newContext({ viewport: { width: 320, height: 184 } });
    await popCtx.addInitScript(() => localStorage.setItem("avdsp-skin", "midnight"));
    const pop = await popCtx.newPage();
    pop.on("pageerror", (e) => fail(`osd-ref popup pageerror: ${e.message}`));
    await pop.goto(url + "?window=popup");
    await pop.waitForSelector(".popup .fine-slider:not([aria-disabled])", { timeout: 4000 });

    const osdCtx = await browser.newContext({ viewport: { width: 280, height: 104 } });
    await osdCtx.addInitScript(() => localStorage.setItem("avdsp-skin", "midnight"));
    p = await osdCtx.newPage();
    p.on("pageerror", (e) => fail(`osd pageerror: ${e.message}`));
    p.on("console", (m) => m.type() === "error" && fail(`osd console: ${m.text()}`));
    await p.goto(url + "?window=osd&db=-6&muted");
    await p.waitForSelector(".osd-value");
    if ((await p.locator(".osd .popup-title").innerText()).trim().toLowerCase() !== "audivane dsp") {
      fail(`osd title missing Audivane DSP: ${await p.locator(".osd .popup-title").innerText()}`);
    }
    if ((await p.locator("button, input, [tabindex]").count()) !== 0) {
      fail("osd must have no buttons or focusable controls");
    }
    const mutedVal = await p.locator(".osd-value").innerText();
    const mutedTag = await p.locator(".osd-muted-tag").innerText();
    if (!/6\.0 dB/.test(mutedVal) || !/muted/i.test(mutedTag)) fail(`osd muted state "${mutedVal}" / "${mutedTag}"`);
    await shot(p, "osd-muted-midnight");

    // Bar fill percentage matches the tray popup's slider for the same levels (including a ceiling).
    for (const [db, maxDb] of [[-40, 0], [-20, 0], [-3, 0], [-10, -10]]) {
      await pop.evaluate(
        ([d, ceil]) => {
          const dm = window.avdsp.transport.daemon;
          dm.config.volume.max_volume_db = ceil === 0 ? null : ceil;
          dm.volume.max_db = ceil;
          dm.setLevel({ volume_db: d, muted: false });
        },
        [db, maxDb],
      );
      await pop.waitForFunction(
        ([d, ceil]) => {
          const v = window.avdsp.getState().status?.volume;
          return v && v.volume_db === d && v.max_db === ceil && !v.muted;
        },
        [db, maxDb],
        { timeout: 3000 },
      );
      await p.evaluate(
        ([d, ceil]) => window.avdspShell.emitShell({ kind: "osd", payload: { volume_db: d, muted: false, min_db: -80, max_db: ceil } }),
        [db, maxDb],
      );
      await p.waitForFunction(
        (d) => (document.querySelector(".osd-value")?.textContent ?? "").includes(`${Math.abs(d).toFixed(1)} dB`),
        db,
        { timeout: 3000 },
      );
      const popPct = await pop.locator("#popup-vol .fine-fill").evaluate((el) => parseFloat(el.style.width));
      const osdPct = await p.locator(".osd-bar .fine-fill").evaluate((el) => parseFloat(el.style.width));
      if (!Number.isFinite(popPct) || !Number.isFinite(osdPct) || Math.abs(popPct - osdPct) > 0.05) {
        fail(`osd bar fill (${osdPct}%) != popup slider (${popPct}%) at ${db} dB (max ${maxDb})`);
      }
    }
    await popCtx.close();

    // Unmuted screenshots in dark (midnight) and light themes at -3.0 dB, plus muted light.
    await p.evaluate(() => window.avdspShell.emitShell({ kind: "osd", payload: { volume_db: -3, muted: false, min_db: -80, max_db: 0 } }));
    await p.waitForFunction(() => /3\.0 dB/.test(document.querySelector(".osd-value")?.textContent ?? ""), null, { timeout: 3000 });
    await shot(p, "osd-midnight");
    await osdCtx.close();

    const lightCtx = await browser.newContext({ viewport: { width: 280, height: 104 } });
    await lightCtx.addInitScript(() => localStorage.setItem("avdsp-skin", "light"));
    p = await lightCtx.newPage();
    p.on("pageerror", (e) => fail(`osd light pageerror: ${e.message}`));
    await p.goto(url + "?window=osd&db=-3");
    await p.waitForSelector(".osd-value");
    await shot(p, "osd-light");
    await p.evaluate(() => window.avdspShell.emitShell({ kind: "osd", payload: { volume_db: -6, muted: true, min_db: -80, max_db: 0 } }));
    await p.waitForSelector(".osd-muted-tag");
    await shot(p, "osd-muted-light");
    await lightCtx.close();
  }

  // --- volume keys and tray wheel reach the main window's store, and the OSD
  p = await page();
  await p.waitForSelector(".app");
  await p.waitForFunction(() => window.avdsp.getState().volumeFresh);
  {
    const v0 = await p.evaluate(() => window.avdsp.getState().status.volume.volume_db);
    await p.evaluate(() => {
      for (let i = 0; i < 3; i++) window.avdspShell.emitShell({ kind: "volume-key", action: "down", step_db: 1, source: "keys" });
    });
    await p.waitForFunction((v) => window.avdsp.getState().status.volume.volume_db === v - 3, v0, { timeout: 3000 }).catch(() => fail("volume keys: three presses did not lower the volume by 3 dB"));
    const osd = await p.evaluate(() => [window.avdspShell.lastOsd, window.avdspShell.shellCalls.filter((c) => c.startsWith("osd:")).length]);
    if (!osd[0] || osd[0].volume_db !== v0 - 3 || osd[1] !== 3) fail(`volume keys: OSD ${JSON.stringify(osd)}`);
  }
  // Device → Tray and volume keys
  await p.getByRole("button", { name: /^Device/ }).click();
  await p.waitForSelector(".tray-panel");
  if ((await p.locator(".tray-panel .osd-where").innerText()).includes("Configure Audio Volume")) {
    fail("OSD help text shows KDE popup tip outside KDE");
  }
  await p.getByLabel("Use the keyboard's volume keys for Audivane").check();
  await p.locator(".tray-panel .keys-status").waitFor({ timeout: 3000 }).catch(() => fail("volume keys: no state after turning them on"));
  if (!(await p.locator(".tray-panel .keys-status").innerText()).includes("Volume up → Volume Up")) fail("volume keys: bound keys not listed");
  await p.locator(".tray-panel").getByRole("button", { name: "Menu" }).click();
  if ((await p.locator(".tray-panel").getByRole("button", { name: "Menu" }).getAttribute("aria-pressed")) !== "true") fail("tray click: Menu not selected");
  await p.locator(".tray-panel").getByRole("button", { name: "2 dB" }).click();
  await p.waitForFunction(() => window.avdspShell.shell.settings.volume_step_db === 2, null, { timeout: 2000 }).catch(() => fail("key step not saved"));
  await p.locator(".tray-panel").scrollIntoViewIfNeeded();
  await shot(p, "device-tray-keys");
  // Reverse wheel is an app setting like the others.
  await p.getByLabel("Reverse wheel direction").check();
  await p.waitForFunction(() => window.avdspShell.shell.settings.reverse_wheel === true, null, { timeout: 2000 }).catch(() => fail("reverse wheel not saved"));
  await p.close();
  // Declined in the desktop's dialog: the text sends the user to the desktop's shortcut settings.
  p = await page("?keys=declined");
  await p.waitForSelector(".app");
  await p.getByRole("button", { name: /^Device/ }).click();
  await p.getByLabel("Use the keyboard's volume keys for Audivane").check();
  await p.locator(".tray-panel .notice.warn").waitFor({ timeout: 3000 }).catch(() => fail("volume keys: declined not reported"));
  {
    const t = await p.locator(".tray-panel .notice.warn").innerText();
    if (!t.includes("System Settings") || /again to be asked/.test(t)) fail(`declined text: ${t}`);
  }
  await p.close();
  p = await page("?keys=none");
  await p.waitForSelector(".app");
  await p.getByRole("button", { name: /^Device/ }).click();
  await p.getByLabel("Use the keyboard's volume keys for Audivane").check();
  await p.locator(".tray-panel .notice.warn").waitFor({ timeout: 3000 }).catch(() => fail("volume keys: no portal not reported"));
  await p.close();
  // On KDE: the On-screen display help text warns to turn off Plasma's own volume popup.
  for (const skin of ["midnight", "light"]) {
    p = await page("?kde", skin);
    await p.waitForSelector(".app");
    await p.getByRole("button", { name: /^Device/ }).click();
    await p.waitForSelector(".tray-panel .osd-where");
    const t = await p.locator(".tray-panel .osd-where").innerText();
    if (!t.includes("Show OSD popups for changes to: Audio volume") || !t.includes("Configure Audio Volume")) {
      fail(`KDE OSD help text missing popup tip (${skin}): ${t}`);
    }
    await p.locator(".tray-panel").scrollIntoViewIfNeeded();
    await shot(p, `device-tray-kde-${skin}`);
    await p.close();
  }
  // Closing the main window to the tray destroys its webview; the page made
  // again comes back on the same view and scroll position (the mock keeps
  // them in session storage, so a reload stands for the window being made
  // again).
  p = await page("", "midnight", [1100, 640]);
  await p.waitForSelector(".app");
  await p.getByRole("button", { name: /^Device/ }).click();
  {
    const room = await p.evaluate(() => {
      const m = document.getElementById("main");
      return m.scrollHeight - m.clientHeight;
    });
    if (room < 200) fail(`main view memory: the device view is too short to scroll (${room}px)`);
    const want = Math.min(420, room);
    // Close right after scrolling, inside the save's debounce: the close
    // handshake must carry the position, then the page is gone.
    await p.evaluate((top) => {
      document.getElementById("main").scrollTop = top;
      window.avdspShell.emitShell({ kind: "main-closing" });
    }, want);
    if (!(await p.evaluate(() => window.avdspShell.shellCalls.includes("main-close-ready")))) fail("main view memory: no answer to main-closing");
    await p.reload();
    await p.waitForSelector(".app");
    await p
      .waitForFunction((top) => Math.abs(document.getElementById("main").scrollTop - top) <= 1, want, { timeout: 3000 })
      .catch(() => fail("main view memory: scroll position not restored"));
    const view = await p.evaluate(() => window.avdsp.getState().view);
    if (view !== "device") fail(`main view memory: came back on ${view}, not device`);
    // Another view: its own position is what comes back.
    await p.evaluate(() => window.avdsp.setView("eq"));
    await p.waitForTimeout(100);
    const eqTop = await p.evaluate(() => {
      const m = document.getElementById("main");
      m.scrollTop = 40;
      window.avdspShell.emitShell({ kind: "main-closing" });
      return m.scrollTop;
    });
    await p.reload();
    await p.waitForSelector(".app");
    await p
      .waitForFunction((top) => window.avdsp.getState().view === "eq" && Math.abs(document.getElementById("main").scrollTop - top) <= 1, eqTop, { timeout: 3000 })
      .catch(() => fail(`main view memory: EQ not back at ${eqTop}`));
  }
  await p.close();
  // --- A/B quick compare: a stage header's Compare sets side B and turns
  // A/B on, the top bar names B, the backquote flips, the A/B view shows
  // B's description, the level panel and overs, and the overload stop.
  for (const skin of ["midnight", "light"]) {
    p = await page("?followers=USB", skin);
    await p.waitForSelector(".app");
    const abOf = () => p.evaluate(() => window.avdsp.getState().config.ab);
    // The Device line mentions the follower.
    const devSub = await p.locator(".side .nav-item", { has: p.locator(".nav-name", { hasText: /^Device$/ }) }).locator(".nav-sub").textContent();
    if (!devSub?.includes("+1 device")) fail(`A/B (${skin}): the Device summary does not mention the follower: ${devSub}`);
    await p.evaluate(async () => {
      await window.avdsp.setAbSource("off");
      window.avdsp.setStageEnabled("crossfeed", true);
      await window.avdsp.whenIdle();
      window.avdsp.setView("crossfeed");
    });
    const cmp = p.locator(".view-head .compare-btn");
    await cmp.waitFor({ timeout: 3000 }).catch(() => fail(`A/B (${skin}): no Compare button in the crossfeed header`));
    await cmp.click();
    await p
      .waitForFunction(() => window.avdsp.getState().config.ab?.label === "Crossfeed off", null, { timeout: 3000 })
      .catch(() => fail(`A/B (${skin}): Compare did not set B = crossfeed off`));
    const ab = await abOf();
    if (JSON.stringify(ab?.b) !== '{"patch":{"crossfeed":{"enabled":false}}}') fail(`A/B (${skin}): side B patch ${JSON.stringify(ab?.b)}`);
    await p.waitForFunction(() => document.querySelector(".compare-btn")?.textContent === "Stop compare", null, { timeout: 2000 }).catch(() => fail(`A/B (${skin}): the button does not read Stop compare`));
    const label = await p.locator(".top .ab-label").textContent().catch(() => null);
    if (label !== "B: Crossfeed off") fail(`A/B (${skin}): top bar label is ${label}`);
    await p.locator("body").click({ position: { x: 5, y: 600 } });
    await p.keyboard.press("Backquote");
    await p.waitForFunction(() => window.avdsp.getState().status.ab?.current === "b", null, { timeout: 2000 }).catch(() => fail(`A/B (${skin}): the backquote did not flip to B`));
    await shot(p, `ab-compare-crossfeed-${skin}`);
    // Every stage page has its button.
    for (const view of ["dc_block", "pitch", "harmonics", "eq", "conv"]) {
      await p.evaluate((v) => window.avdsp.setView(v), view);
      if ((await p.locator(".view-head .compare-btn").count()) !== 1) fail(`A/B (${skin}): no Compare button on ${view}`);
    }
    // The A/B view.
    await p.evaluate(() => window.avdspShell.daemon.simulateAbOvers(0, 3));
    await p.locator(".top .ab-label").click();
    await p.waitForSelector("#ab-title");
    const view = (await p.locator(".view").textContent()) ?? "";
    for (const want of ["Crossfeed off", "A with crossfeed switched off", "B offset", "A gain", "B gain", "Headroom", "B was limited 3×", "not clean", "aligned"]) {
      if (!view.includes(want)) fail(`A/B (${skin}): the A/B view lacks "${want}"`);
    }
    await p.getByRole("group", { name: "Shared headroom" }).getByRole("button", { name: "Fixed" }).click();
    await p.waitForFunction(() => window.avdsp.getState().config.ab?.headroom.mode === "fixed", null, { timeout: 2000 }).catch(() => fail(`A/B (${skin}): Fixed headroom not sent`));
    await p.getByLabel("Headroom", { exact: true }).fill("4");
    await p.getByLabel("Headroom", { exact: true }).press("Enter");
    await p.waitForFunction(() => window.avdsp.getState().config.ab?.headroom.db === 4, null, { timeout: 2000 }).catch(() => fail(`A/B (${skin}): headroom 4 dB not sent`));
    await p.evaluate(() => window.avdspShell.daemon.simulateAbOvers(0, 3));
    await p.waitForSelector(".ab-overs", { timeout: 2000 }).catch(() => fail(`A/B (${skin}): overs not shown`));
    await shot(p, `ab-view-${skin}`);
    // Match to this track: progress and the running offset, then a lock; a
    // track change mid-run cancels with a reason.
    await p.getByRole("button", { name: "Match to this track" }).click();
    await p.waitForSelector('.ab-match [role="progressbar"]', { timeout: 2000 }).catch(() => fail(`A/B (${skin}): no measurement progress`));
    await p.waitForFunction(() => /Listening: \d+ s of the 20 s minimum/.test(document.querySelector(".ab-match")?.textContent ?? ""), null, { timeout: 3000 }).catch(() => fail(`A/B (${skin}): no minimum progress text`));
    if (!(await p.getByRole("button", { name: "Cancel" }).count())) fail(`A/B (${skin}): no Cancel while measuring`);
    await shot(p, `ab-measuring-${skin}`);
    await p.waitForFunction(() => window.avdsp.getState().config.ab?.level.mode === "measured", null, { timeout: 8000 }).catch(() => fail(`A/B (${skin}): the measurement did not lock`));
    await p.waitForFunction(() => (document.querySelector(".view")?.textContent ?? "").includes("Measured: B"), null, { timeout: 2000 }).catch(() => fail(`A/B (${skin}): the level panel does not show the measured offset`));
    if ((await p.getByRole("group", { name: "Level match" }).getByRole("button", { name: "Measured" }).getAttribute("aria-pressed")) !== "true") fail(`A/B (${skin}): the Measured segment is not pressed`);
    await shot(p, `ab-measured-${skin}`);
    await p.getByRole("button", { name: "Measure again" }).click();
    await p.waitForSelector('.ab-match [role="progressbar"]', { timeout: 2000 });
    await p.evaluate(() => window.avdspShell.daemon.simulateTrackChange());
    await p.waitForSelector('.ab-match .inline-err', { timeout: 2000 }).catch(() => fail(`A/B (${skin}): no cancel reason`));
    if (!((await p.locator(".ab-match .inline-err").textContent()) ?? "").includes("Cancelled: the track changed.")) fail(`A/B (${skin}): cancel reason text`);
    await shot(p, `ab-measure-cancelled-${skin}`);
    // The overload fallback turns A/B off: the notice, the top bar and the sidebar say so.
    await p.evaluate(() => window.avdspShell.daemon.simulateAbOverload(1.5));
    await p.waitForSelector(".view .notice.warn", { timeout: 2000 }).catch(() => fail(`A/B (${skin}): no overload notice`));
    if (!((await p.locator(".view .notice.warn").textContent()) ?? "").includes("turned A/B off")) fail(`A/B (${skin}): overload notice text`);
    if ((await p.locator(".top .ab-label").textContent()) !== "A/B off: overload") fail(`A/B (${skin}): top bar does not show the overload stop`);
    await shot(p, `ab-overload-${skin}`);
    // A filter compare from the Upsampling view is a new side B and ends the stop.
    await p.evaluate(() => window.avdsp.setView("ups"));
    const pick = p.getByLabel("Compare with another filter");
    await pick.waitFor({ timeout: 3000 }).catch(() => fail(`A/B (${skin}): no filter compare on Upsampling`));
    const value = await pick.evaluate((el) => [...el.querySelectorAll("option")].find((o) => o.value && !o.disabled && o.value !== "recommended")?.value);
    await pick.selectOption(value);
    await p
      .waitForFunction(() => window.avdsp.getState().config.ab?.label?.startsWith("Filter: ") && !window.avdsp.getState().status.overload, null, { timeout: 3000 })
      .catch(() => fail(`A/B (${skin}): the filter compare did not set B or end the stop`));
    await p.waitForSelector(".view-head .compare-btn.on", { timeout: 2000 }).catch(() => fail(`A/B (${skin}): no Stop compare for the filter`));
    await shot(p, `ab-filter-${skin}`);
    await p.locator(".view-head .compare-btn.on").click();
    await p.waitForFunction(() => window.avdsp.getState().config.ab === null, null, { timeout: 2000 }).catch(() => fail(`A/B (${skin}): Stop compare did not turn A/B off`));
    if (await p.locator(".top .ab-label").count()) fail(`A/B (${skin}): the top bar still names B with A/B off`);
    await p.close();
  }
} finally {
  await browser.close();
  await server.close();
}
if (errors.length) process.exit(1);
console.log("interaction checks passed");
