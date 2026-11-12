import test from "node:test";
import assert from "node:assert/strict";
import { deviceLoginView } from "../desktop/ui/device-login.mjs";

test("device prompt is shown only for its pending sign-in", () => {
  const prompt = { verification_url: "https://auth.openai.com/codex/device", user_code: "TEST-CODE", expires_at: 1000 };
  const state = { login: { pending: true, method: "device_code" }, device_code: prompt };
  assert.equal(deviceLoginView(state, 940000).code, "TEST-CODE");
  assert.match(deviceLoginView(state, 940000).expiry, /1:00/);
  assert.equal(deviceLoginView({ ...state, login: { pending: false, method: "device_code" } }).code, "");
  assert.equal(deviceLoginView({ ...state, login: { pending: true, method: "browser" } }).code, "");
});

test("requesting, expiry and old browser status have clear instructions", () => {
  assert.match(deviceLoginView({ login: { pending: true, method: "device_code" } }).description, /Получаем/);
  assert.match(deviceLoginView({ login: { pending: true, method: "device_code" }, device_code: { user_code: "A", expires_at: 1 } }, 2000).expiry, /истёк/);
  assert.match(deviceLoginView({ login: { pending: true } }).title, /браузере/);
});
