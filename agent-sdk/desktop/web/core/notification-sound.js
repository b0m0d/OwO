/* Optional, dependency-free notification tones for the workbench. */
(function (global) {
  "use strict";

  function createNotificationSound(options = {}) {
    const AudioContextCtor = options.AudioContext ||
      (global && (global.AudioContext || global.webkitAudioContext));
    let context = null;

    function getContext() {
      if (!AudioContextCtor) return null;
      if (!context) context = new AudioContextCtor();
      return context;
    }

    function unlock(enabled) {
      if (!enabled) return false;
      const audio = getContext();
      if (!audio) return false;
      if (audio.state === "suspended" && typeof audio.resume === "function") {
        try {
          const pending = audio.resume();
          if (pending && typeof pending.catch === "function") pending.catch(() => undefined);
        } catch (_) {
          return false;
        }
      }
      return true;
    }

    function play(enabled) {
      if (!enabled || !unlock(true)) return false;
      const audio = getContext();
      if (!audio || typeof audio.createOscillator !== "function" || typeof audio.createGain !== "function") {
        return false;
      }
      const now = audio.currentTime;
      const oscillator = audio.createOscillator();
      const gain = audio.createGain();
      oscillator.type = "sine";
      oscillator.frequency.setValueAtTime(880, now);
      oscillator.frequency.exponentialRampToValueAtTime(660, now + 0.14);
      gain.gain.setValueAtTime(0.0001, now);
      gain.gain.linearRampToValueAtTime(0.12, now + 0.015);
      gain.gain.exponentialRampToValueAtTime(0.0001, now + 0.18);
      oscillator.connect(gain);
      gain.connect(audio.destination);
      oscillator.start(now);
      oscillator.stop(now + 0.19);
      return true;
    }

    return Object.freeze({ unlock, play });
  }

  const defaultSound = createNotificationSound();
  global.OwoNotificationSound = Object.freeze({
    unlock: defaultSound.unlock,
    play: defaultSound.play,
    createNotificationSound,
  });
})(typeof window !== "undefined" ? window : globalThis);
