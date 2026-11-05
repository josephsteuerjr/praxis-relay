import assert from "node:assert/strict";
import fs from "node:fs/promises";
import path from "node:path";
import { pathToFileURL, fileURLToPath } from "node:url";
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const { chromium } = await import(process.env.PLAYWRIGHT_MODULE
  ? pathToFileURL(process.env.PLAYWRIGHT_MODULE).href : "playwright");
const browser = await chromium.launch({ headless: true, channel: "msedge" });
const checks = [];
try {
  const page = await browser.newPage({ viewport: { width: 1120, height: 760 } });
  const errors = [];
  page.on("pageerror", error => errors.push(String(error)));
  await page.route("http://relay-fixture.test/**", async route => {
    const name = decodeURIComponent(new URL(route.request().url()).pathname).replace(/^\//, "") || "index.html";
    const file = path.resolve(root, "desktop/ui", name);
    if (!file.startsWith(path.join(root, "desktop/ui") + path.sep)) return route.fulfill({ status: 404, body: "" });
    try {
      const body = await fs.readFile(file);
      const type = /\.(?:js|mjs)$/.test(name) ? "text/javascript" : name.endsWith(".css") ? "text/css" : name.endsWith(".html") ? "text/html" : "application/octet-stream";
      await route.fulfill({ body, contentType: type });
    } catch { await route.fulfill({ status: 404, body: "" }); }
  });
  await page.addInitScript(() => {
    window.fixture = {
      version: "1.1.2-rc.1", running: false, accounts: [], events: [],
      login: { pending: false }, auth_directory: "fixture/data", base_url: "http://127.0.0.1:5011/v1",
      environment_overrides: [], instructions_file: null,
      settings: {
        port: 5011, quota_refresh_seconds: 0, labels: { primary: "First", secondary: "Second" },
        appearance: { theme: "light", paper_light: "cream", paper_dark: "coal", accent: "terra", scale: 100, density: "comfortable", handwritten: false, grain: false, reduced_motion: true },
        defaults: { model: "first", reasoning: "default", instructions_mode: "codex", instructions: "", parallel_tools: true },
      },
    };
    window.__TAURI__ = { core: { invoke: async (command, args) => {
      if (command === "window_command") return {};
      if (args.op === "models") return {
        source: "live", models: ["first", "second", "unknown-metadata"],
        data: [{ id: "first", reasoning_efforts: ["none", "low", "high"] },
          { id: "second", reasoning_efforts: ["medium", "ultra"] }, { id: "unknown-metadata" }],
      };
      if (args.op === "configure") {
        const merge = (target, patch) => {
          for (const [key, value] of Object.entries(patch)) {
            if (value && typeof value === "object" && !Array.isArray(value)) merge(target[key] ||= {}, value);
            else target[key] = value;
          }
        };
        merge(window.fixture.settings, args.settings);
      }
      return structuredClone(window.fixture);
    } } };
  });
  await page.goto("http://relay-fixture.test/");
  const values = () => page.locator("#reasoning option").evaluateAll(nodes => nodes.map(node => node.value));
  await page.waitForFunction(() => document.querySelectorAll("#reasoning option").length === 3);
  assert.deepEqual(await values(), ["default", "low", "high"]);
  checks.push("normal desktop entry loads capability-driven choices and hides none");
  await page.locator("[data-page=settings]").click();
  await page.locator("[data-settings-tab=model]").click();
  await page.locator("#default-model").fill("second");
  assert.deepEqual(await values(), ["default", "medium", "ultra"]);
  await page.locator("#reasoning").selectOption("ultra");
  await page.waitForFunction(() => window.fixture.settings.defaults.reasoning === "ultra" && window.fixture.settings.defaults.model === "second");
  checks.push("selected model changes visible levels and explicit ultra persists");
  await page.locator("#default-model").fill("unknown-metadata");
  const unknown = await values();
  assert(unknown.includes("default") && unknown.every(value => value === "" || value === "default"));
  checks.push("unknown metadata offers only model default and an explicit unavailable-selection prompt");
  await page.evaluate(() => { window.fixture.settings.defaults.reasoning = "none"; });
  await page.waitForFunction(() => document.querySelector("#reasoning-hint").textContent.includes("ранее сохранённого"));
  assert.equal(await page.locator("#reasoning").inputValue(), "default");
  assert.equal(await page.evaluate(() => window.fixture.settings.defaults.reasoning), "none");
  checks.push("legacy none is explained as model default while raw settings remain untouched");
  assert.deepEqual(errors, []);
  if (process.env.RELAY_UI_SCREENSHOT) await page.screenshot({ path: process.env.RELAY_UI_SCREENSHOT, fullPage: true });
  console.log(JSON.stringify({ passed: true, checks }, null, 2));
} finally { await browser.close(); }
