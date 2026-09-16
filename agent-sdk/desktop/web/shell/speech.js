// §12.3 shell 拆分批次一：语音输入（自 app.js 机械外移，零行为变化）。
// 顶层 let/function 声明经经典脚本全局词法环境供 app.js 裸名引用
// （app.js 的麦克风流程直接操作 recognition/listening/localRecorder，故不可 IIFE 私有化）。

"use strict";

let recognition = null;
let listening = false;
let localRecorder = null;

function initSpeech() {
  const SpeechRecognition = window.SpeechRecognition || window.webkitSpeechRecognition;
  if (!SpeechRecognition) return;
  recognition = new SpeechRecognition();
  recognition.lang = "zh-CN";
  recognition.continuous = false;
  recognition.interimResults = false;
  recognition.onresult = (event) => {
    const text = event.results[0][0].transcript;
    const prompt = document.getElementById("prompt");
    prompt.value = (prompt.value ? prompt.value + " " : "") + text;
  };
  recognition.onend = () => {
    listening = false;
    document.getElementById("micBtn").textContent = "🎤";
  };
}

function encodeWav(samples, sampleRate) {
  const buffer = new ArrayBuffer(44 + samples.length * 2);
  const view = new DataView(buffer);
  const writeString = (offset, str) => {
    for (let i = 0; i < str.length; i++) view.setUint8(offset + i, str.charCodeAt(i));
  };
  writeString(0, "RIFF");
  view.setUint32(4, 36 + samples.length * 2, true);
  writeString(8, "WAVE");
  writeString(12, "fmt ");
  view.setUint32(16, 16, true);
  view.setUint16(20, 1, true);
  view.setUint16(22, 1, true);
  view.setUint32(24, sampleRate, true);
  view.setUint32(28, sampleRate * 2, true);
  view.setUint16(32, 2, true);
  view.setUint16(34, 16, true);
  writeString(36, "data");
  view.setUint32(40, samples.length * 2, true);
  let offset = 44;
  for (const sample of samples) {
    const clamped = Math.max(-1, Math.min(1, sample));
    view.setInt16(offset, clamped * 0x7fff, true);
    offset += 2;
  }
  return new Blob([buffer], { type: "audio/wav" });
}

// 本地优先语音输入：麦克风 → 16k WAV → /stt/transcribe（SenseVoice-Small）。
// 本地 STT 不可用时回退到系统 Web Speech。
async function startLocalRecording() {
  const AudioCtx = window.AudioContext || window.webkitAudioContext;
  if (!AudioCtx) return false;
  let stream;
  try {
    stream = await navigator.mediaDevices.getUserMedia({ audio: true });
  } catch (_) {
    return false;
  }
  const context = new AudioCtx({ sampleRate: 16000 });
  const source = context.createMediaStreamSource(stream);
  const processor = context.createScriptProcessor(4096, 1, 1);
  const chunks = [];
  source.connect(processor);
  processor.connect(context.destination);
  processor.onaudioprocess = (event) => {
    chunks.push(new Float32Array(event.inputBuffer.getChannelData(0)));
  };
  localRecorder = {
    stop: async () => {
      source.disconnect();
      processor.disconnect();
      stream.getTracks().forEach((track) => track.stop());
      const sampleRate = context.sampleRate;
      await context.close();
      const samples = [];
      for (const chunk of chunks) samples.push(...chunk);
      return { blob: encodeWav(samples, sampleRate), sampleCount: samples.length };
    },
  };
  return true;
}
