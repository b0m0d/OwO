import test from "node:test";
import assert from "node:assert/strict";
import { runInNewContext } from "node:vm";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const source = readFileSync(join(here, "..", "core", "notification-sound.js"), "utf8");

function load(createContext) {
  const window = {};
  runInNewContext(source, { window });
  assert.equal(typeof window.OwoNotificationSound.unlock, "function");
  assert.equal(typeof window.OwoNotificationSound.play, "function");
  return window.OwoNotificationSound.createNotificationSound({ AudioContext: createContext });
}

test("notification sound stays silent until enabled and unlocks from the settings gesture", () => {
  let constructed = 0;
  class FakeAudioContext {
    constructor() { constructed += 1; this.state = "suspended"; this.currentTime = 1; this.destination = {}; }
    resume() { this.state = "running"; }
  }
  const sound = load(FakeAudioContext);
  assert.equal(sound.play(false), false);
  assert.equal(constructed, 0);
  assert.equal(sound.unlock(true), true);
  assert.equal(constructed, 1);
});

test("notification sound plays a short, bounded tone only when enabled", () => {
  const calls = [];
  class FakeAudioContext {
    constructor() {
      this.state = "running";
      this.currentTime = 2;
      this.destination = {};
    }
    createOscillator() {
      return {
        frequency: {
          setValueAtTime: (...args) => calls.push(["frequency", ...args]),
          exponentialRampToValueAtTime: (...args) => calls.push(["frequencyRamp", ...args]),
        },
        connect: (target) => calls.push(["oscillatorConnect", target]),
        start: (time) => calls.push(["start", time]),
        stop: (time) => calls.push(["stop", time]),
      };
    }
    createGain() {
      return {
        gain: {
          setValueAtTime: (...args) => calls.push(["gain", ...args]),
          linearRampToValueAtTime: (...args) => calls.push(["gainRamp", ...args]),
          exponentialRampToValueAtTime: (...args) => calls.push(["gainEnd", ...args]),
        },
        connect: (target) => calls.push(["gainConnect", target]),
      };
    }
  }
  const sound = load(FakeAudioContext);
  assert.equal(sound.play(false), false);
  assert.equal(sound.play(true), true);
  assert.equal(calls.some(([name]) => name === "start"), true);
  assert.deepEqual(calls.find(([name]) => name === "stop"), ["stop", 2.19]);
});


test("settings expose only preferences that are wired to runtime behavior", () => {
  const index = readFileSync(join(here, "..", "index.html"), "utf8");
  const app = readFileSync(join(here, "..", "app.js"), "utf8");
  const domain = readFileSync(join(here, "..", "app-domain.js"), "utf8");
  for (const id of ["prefShell", "prefLanguage", "prefSpeed", "prefTone", "prefReminder", "prefApprovalPin", "prefCompactAppearance"]) {
    assert.doesNotMatch(index, new RegExp('id="' + id + '"'), id + " is not a runtime-backed preference");
  }
  assert.match(index, /id="prefSound"/);
  assert.match(app, /key === "sound" && LOCAL_PREFS\.sound[\s\S]{0,80}OwoNotificationSound/);
  assert.match(app, /pointerdown[\s\S]{0,120}unlockNotificationAudioIfEnabled/);
  assert.match(domain, /case "permission_request":[\s\S]{0,180}OwoNotificationSound/);
  assert.match(domain, /completionStatus !== "aborted"[\s\S]{0,100}OwoNotificationSound/);
});
