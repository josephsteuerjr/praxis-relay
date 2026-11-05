import { test } from "node:test";
import assert from "node:assert/strict";
import { reasoningChoices } from "../desktop/ui/reasoning-options.mjs";
import { readFile } from "node:fs/promises";

const catalog = [
  { id: "first", reasoning_efforts: ["none", "low", "medium", "high", "high"] },
  { id: "second", reasoning_efforts: ["medium", "xhigh", "ultra"] },
  { id: "unknown-metadata" },
];
test("model changes show exactly its supported levels without none", () => {
  assert.deepEqual(reasoningChoices(catalog, "first"), ["default", "low", "medium", "high"]);
  assert.deepEqual(reasoningChoices(catalog, "second"), ["default", "medium", "xhigh", "ultra"]);
  assert.deepEqual(reasoningChoices(catalog, ""), ["default", "low", "medium", "high"]);
});
test("missing or unknown catalog offers model default only", () => {
  for (const name of ["not-listed", "unknown-metadata"]) {
    assert.deepEqual(reasoningChoices(catalog, name), ["default"]);
  }
  assert.deepEqual(reasoningChoices([], ""), ["default"]);
  assert.deepEqual(reasoningChoices([{id:"a",reasoning_efforts:["none","future-mode",7]}], "a"), ["default"]);
});
test("desktop normal entry imports dynamic choices and contains no static none option", async () => {
  const html = await readFile(new URL("../desktop/ui/index.html", import.meta.url), "utf8");
  const app = await readFile(new URL("../desktop/ui/app.js", import.meta.url), "utf8");
  assert.match(html, /<script type="module" src="app.js"><\/script>/);
  assert.doesNotMatch(html, /<option value="none">/);
  assert.match(app, /import \{ reasoningChoices \} from "\.\/reasoning-options\.mjs"/);
  assert.match(app, /event\.target\.dataset\.setting === "defaults\.model"/);
});
