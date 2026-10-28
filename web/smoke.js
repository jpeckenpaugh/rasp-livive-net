// D-04 browser smoke: vendored .weba -> Rust decode in wasm -> plain
// AudioBufferSourceNode. No DSP, no worklet, no resampling code.
//
// This module is intentionally dependency-free (no framework, no CDN) and
// fails loudly: every failed clip is shown with its id and error message.

import init, { version, decode } from "./wasm/rasp.js";

const els = {
  fatal: document.getElementById("fatal"),
  fatalMessage: document.getElementById("fatal-message"),
  stVersion: document.getElementById("st-version"),
  stWasm: document.getElementById("st-wasm"),
  stInit: document.getElementById("st-init"),
  stRate: document.getElementById("st-rate"),
  stRegistry: document.getElementById("st-registry"),
  voice: document.getElementById("voice"),
  phrase: document.getElementById("phrase"),
  decode: document.getElementById("decode"),
  play: document.getElementById("play"),
  status: document.getElementById("status"),
  stClip: document.getElementById("st-clip"),
  stFrames: document.getElementById("st-frames"),
  stDuration: document.getElementById("st-duration"),
  stFormat: document.getElementById("st-format"),
  stDecode: document.getElementById("st-decode"),
  stPeak: document.getElementById("st-peak"),
  decodeAll: document.getElementById("decode-all"),
  progress: document.getElementById("all-progress"),
  allStatus: document.getElementById("all-status"),
  stAllClips: document.getElementById("st-all-clips"),
  stAllAudio: document.getElementById("st-all-audio"),
  stAllDecode: document.getElementById("st-all-decode"),
  stAllThroughput: document.getElementById("st-all-throughput"),
  stAllRate: document.getElementById("st-all-rate"),
  stAllWall: document.getElementById("st-all-wall"),
  errorsWrap: document.getElementById("all-errors-wrap"),
  errorsSummary: document.getElementById("all-errors-summary"),
  errors: document.getElementById("errors"),
};

const state = {
  registry: null,
  /** The decoded clip currently shown: {voice, phrase, entry, samples, ...}. */
  current: null,
  /** Incremented per selection so a stale decode cannot overwrite a newer one. */
  decodeSeq: 0,
  /** True while the full-corpus run is in flight. */
  busyAll: false,
  audioContext: null,
};

const SAMPLE_RATE = 48_000;

// --- Small helpers ----------------------------------------------------------

function setText(el, text) {
  el.textContent = text;
}

function fmtBytes(bytes) {
  return `${bytes.toLocaleString()} B (${(bytes / 1024).toFixed(1)} KiB)`;
}

function fmtMs(ms) {
  return `${ms.toFixed(2)} ms`;
}

function msPerSecondOfAudio(totalMs, seconds) {
  return seconds > 0 ? totalMs / seconds : NaN;
}

function median(values) {
  if (values.length === 0) return NaN;
  const sorted = [...values].sort((a, b) => a - b);
  const mid = Math.floor(sorted.length / 2);
  return sorted.length % 2 === 0
    ? (sorted[mid - 1] + sorted[mid]) / 2
    : sorted[mid];
}

function describeError(error) {
  if (error && error.name === "RaspDecodeError") {
    return `${error.name}${error.code ? `(${error.code})` : ""}: ${error.message}`;
  }
  if (error instanceof Error) return `${error.name}: ${error.message}`;
  return String(error);
}

function setStatus(text, kind = "") {
  els.status.textContent = text;
  els.status.className = kind;
}

function setBusy(busy) {
  els.decode.disabled = busy;
  els.play.disabled = busy;
  els.decodeAll.disabled = busy;
}

// --- AudioContext -----------------------------------------------------------

function ensureAudioContext() {
  if (!state.audioContext) {
    const Ctor = window.AudioContext || window.webkitAudioContext;
    if (!Ctor) throw new Error("AudioContext is not available in this browser");
    state.audioContext = new Ctor();
    setText(els.stRate, `${state.audioContext.sampleRate} Hz (browser context)`);
  }
  if (state.audioContext.state === "suspended") {
    state.audioContext.resume().catch(() => {
      // Playing requires a gesture; the caller awaits resume() and reports it.
    });
  }
  return state.audioContext;
}

// --- Module + registry boot -------------------------------------------------

async function loadWasmSize() {
  const url = new URL("./wasm/rasp_bg.wasm", window.location.href);
  const response = await fetch(url, { cache: "no-cache" });
  if (!response.ok) {
    throw new Error(`HTTP ${response.status} fetching ${url.pathname}`);
  }
  const bytes = new Uint8Array(await response.arrayBuffer());

  let gzip = null;
  if (typeof CompressionStream === "function") {
    try {
      const compressed = new Blob([bytes])
        .stream()
        .pipeThrough(new CompressionStream("gzip"));
      gzip = (await new Response(compressed).arrayBuffer()).byteLength;
    } catch {
      gzip = null; // reported as "gzip n/a", never guessed
    }
  }

  setText(
    els.stWasm,
    gzip === null
      ? `${fmtBytes(bytes.length)} raw · gzip size unavailable in this browser`
      : `${fmtBytes(bytes.length)} raw · ${fmtBytes(gzip)} gzip`,
  );
}

function fillSelect(select, items, toLabel, selectedValue) {
  select.replaceChildren();
  for (const item of items) {
    const option = document.createElement("option");
    option.value = item.value;
    option.textContent = toLabel(item.data);
    if (item.value === selectedValue) option.selected = true;
    select.append(option);
  }
}

async function loadRegistry() {
  const response = await fetch("./registry.json", { cache: "no-cache" });
  if (!response.ok) {
    throw new Error(`HTTP ${response.status} fetching registry.json`);
  }
  const registry = await response.json();
  state.registry = registry;

  const voices = registry.voices.map((voice) => ({
    value: voice.id,
    data: voice,
  }));
  const defaultVoice =
    registry.defaultVoice && registry.files[registry.defaultVoice]
      ? registry.defaultVoice
      : voices[0]?.value;
  fillSelect(
    els.voice,
    voices,
    (voice) =>
      `${voice.id} · ${voice.language} · ${voice.gender} · grade ${voice.grade}`,
    defaultVoice,
  );

  const phrases = registry.phrases.map((phrase) => ({
    value: phrase.id,
    data: phrase,
  }));
  const defaultPhrase = phrases.some((phrase) => phrase.value === "greet")
    ? "greet"
    : phrases[0]?.value;
  fillSelect(
    els.phrase,
    phrases,
    (phrase) => `${phrase.text} (${phrase.id}) · ${phrase.group}`,
    defaultPhrase,
  );

  const build = registry.build ?? {};
  const totals = build.totals ?? {};
  setText(
    els.stRegistry,
    `build ${build.id ?? "?"} · ${totals.clips ?? "?"} clips · ` +
      `${totals.voices ?? "?"} voices · ${totals.phrases ?? "?"} phrases`,
  );
}

async function boot() {
  const started = performance.now();
  await init();
  setText(els.stInit, fmtMs(performance.now() - started));
  setText(els.stVersion, version());

  // Independent fetches: the size report must not depend on the registry.
  await Promise.all([loadWasmSize(), loadRegistry()]);

  els.decode.addEventListener("click", () => {
    void decodeSelection(els.voice.value, els.phrase.value);
  });
  els.play.addEventListener("click", () => {
    void playSelected();
  });
  els.decodeAll.addEventListener("click", () => {
    void decodeAll();
  });
  els.voice.addEventListener("change", onSelectionChange);
  els.phrase.addEventListener("change", onSelectionChange);

  setBusy(false);
  // Decode-on-select: the initial selection is decoded without playing.
  void decodeSelection(els.voice.value, els.phrase.value);
}

function onSelectionChange() {
  state.current = null;
  showEmptyClip();
  void decodeSelection(els.voice.value, els.phrase.value);
}

// --- One clip ---------------------------------------------------------------

function showEmptyClip() {
  setText(els.stClip, "—");
  setText(els.stFrames, "—");
  setText(els.stDuration, "—");
  setText(els.stFormat, "—");
  setText(els.stDecode, "—");
  setText(els.stPeak, "—");
}

function showClip(item) {
  setText(els.stClip, `${item.voice}/${item.phrase} · ${item.entry.path}`);
  setText(
    els.stFrames,
    `${item.frames.toLocaleString()} frames · ${item.bytes.toLocaleString()} B file`,
  );
  setText(els.stDuration, `${(item.frames / item.rate).toFixed(3)} s`);
  setText(
    els.stFormat,
    `${item.rate} Hz · ${item.channels} channel${item.channels === 1 ? "" : "s"} · f32`,
  );
  setText(els.stDecode, `${fmtMs(item.decodeMs)} (${fmtMs(msPerSecondOfAudio(item.decodeMs, item.frames / item.rate))} per second of audio)`);
  setText(els.stPeak, item.peak.toFixed(4));
}

async function decodeSelection(voice, phrase) {
  const entry = state.registry?.files?.[voice]?.[phrase];
  const seq = ++state.decodeSeq;
  if (!entry) {
    setStatus(`No registry entry for ${voice}/${phrase}.`, "error");
    return null;
  }

  setBusy(true);
  setStatus(`Decoding ${voice}/${phrase}…`);
  try {
    const response = await fetch(`./${entry.path}`, { cache: "no-cache" });
    if (!response.ok) {
      throw new Error(
        `HTTP ${response.status} ${response.statusText} for ${entry.path}`,
      );
    }
    const bytes = new Uint8Array(await response.arrayBuffer());
    if (typeof entry.bytes === "number" && bytes.length !== entry.bytes) {
      throw new Error(
        `registry says ${entry.bytes} B for ${entry.path}, fetched ${bytes.length} B`,
      );
    }

    const t0 = performance.now();
    const decoded = decode(bytes);
    const decodeMs = performance.now() - t0;

    let samples;
    let frames;
    let channels;
    let rate;
    try {
      samples = decoded.samples();
      frames = decoded.length();
      channels = decoded.channels();
      rate = decoded.sample_rate();
    } finally {
      decoded.free();
    }

    if (rate !== SAMPLE_RATE) {
      throw new Error(`decoder returned ${rate} Hz, expected ${SAMPLE_RATE} Hz`);
    }
    if (frames <= 0) throw new Error(`decoder returned ${frames} frames`);
    if (samples.length !== frames * channels) {
      throw new Error(
        `interleaved length ${samples.length} != ${frames} frames × ${channels} channels`,
      );
    }

    let peak = 0;
    for (let i = 0; i < samples.length; i += 1) {
      const value = samples[i];
      if (!Number.isFinite(value)) {
        throw new Error(`non-finite sample at index ${i}`);
      }
      const magnitude = Math.abs(value);
      if (magnitude > peak) peak = magnitude;
    }

    if (seq !== state.decodeSeq) return null; // a newer selection won

    const item = {
      voice,
      phrase,
      entry,
      samples,
      frames,
      channels,
      rate,
      bytes: bytes.length,
      decodeMs,
      peak,
    };
    state.current = item;
    showClip(item);
    setStatus(
      `Decoded ${voice}/${phrase}: ${frames.toLocaleString()} frames ` +
        `(${(frames / rate).toFixed(3)} s) in ${fmtMs(decodeMs)}.`,
      "ok",
    );
    return item;
  } catch (error) {
    if (seq === state.decodeSeq) {
      state.current = null;
      showEmptyClip();
      setStatus(`${voice}/${phrase}: ${describeError(error)}`, "error");
    }
    return null;
  } finally {
    if (seq === state.decodeSeq) setBusy(false);
  }
}

async function playSelected() {
  const voice = els.voice.value;
  const phrase = els.phrase.value;

  // Create/resume inside the click gesture, before any await.
  let ctx;
  try {
    ctx = ensureAudioContext();
  } catch (error) {
    setStatus(describeError(error), "error");
    return;
  }

  let item = state.current;
  if (!item || item.voice !== voice || item.phrase !== phrase) {
    item = await decodeSelection(voice, phrase);
  }
  if (!item) return;

  try {
    await ctx.resume();
  } catch (error) {
    setStatus(`AudioContext could not start: ${describeError(error)}`, "error");
    return;
  }

  // The buffer is at the decoded 48 kHz; `AudioBufferSourceNode` performs the
  // rate conversion to the context rate. No resampler here (D-03 owns that).
  const buffer = ctx.createBuffer(item.channels, item.frames, item.rate);
  if (item.channels === 1) {
    buffer.copyToChannel(item.samples, 0);
  } else {
    for (let channel = 0; channel < item.channels; channel += 1) {
      const plane = new Float32Array(item.frames);
      for (let frame = 0; frame < item.frames; frame += 1) {
        plane[frame] = item.samples[frame * item.channels + channel];
      }
      buffer.copyToChannel(plane, channel);
    }
  }

  const source = ctx.createBufferSource();
  source.buffer = buffer;
  source.connect(ctx.destination);
  source.onended = () => source.disconnect();
  source.start();

  setText(els.stRate, `${ctx.sampleRate} Hz (browser context)`);
  setStatus(
    `Playing ${voice}/${phrase}: ${item.frames.toLocaleString()} frames at ` +
      `${item.rate} Hz, resampled by the browser to ${ctx.sampleRate} Hz.`,
    "ok",
  );
}

// --- All 1404 clips ---------------------------------------------------------

function allClips() {
  const entries = [];
  for (const [voice, phrases] of Object.entries(state.registry.files)) {
    for (const [phrase, entry] of Object.entries(phrases)) {
      entries.push({ voice, phrase, entry });
    }
  }
  entries.sort((a, b) => {
    const left = `${a.voice}/${a.phrase}`;
    const right = `${b.voice}/${b.phrase}`;
    return left < right ? -1 : left > right ? 1 : 0;
  });
  return entries;
}

async function decodeAll() {
  if (state.busyAll || !state.registry) return;
  state.busyAll = true;
  setBusy(true);
  els.errorsWrap.hidden = true;
  els.errors.textContent = "";
  setText(els.errorsSummary, "Failures");
  els.progress.hidden = false;
  els.progress.value = 0;

  // This is a click, so creating the context is allowed and warning-free. It
  // is used only to report the sample rate the browser actually runs at.
  let ctx = null;
  try {
    ctx = ensureAudioContext();
  } catch {
    ctx = null;
  }

  const entries = allClips();
  els.progress.max = entries.length;
  const expected = state.registry.build?.totals?.clips;
  const countMismatch =
    typeof expected === "number" && expected !== entries.length
      ? `registry totals say ${expected} clips, walked ${entries.length}`
      : null;

  const timings = [];
  const errors = [];
  let passed = 0;
  let framesTotal = 0;
  const wallStart = performance.now();

  for (let index = 0; index < entries.length; index += 1) {
    const { voice, phrase, entry } = entries[index];
    const id = `${voice}/${phrase}`;
    try {
      const response = await fetch(`./${entry.path}`, { cache: "no-cache" });
      if (!response.ok) {
        throw new Error(
          `HTTP ${response.status} ${response.statusText} for ${entry.path}`,
        );
      }
      const bytes = new Uint8Array(await response.arrayBuffer());
      if (typeof entry.bytes === "number" && bytes.length !== entry.bytes) {
        throw new Error(
          `registry says ${entry.bytes} B, fetched ${bytes.length} B`,
        );
      }

      const t0 = performance.now();
      const decoded = decode(bytes);
      const decodeMs = performance.now() - t0;

      try {
        const frames = decoded.length();
        const channels = decoded.channels();
        const rate = decoded.sample_rate();
        if (rate !== SAMPLE_RATE) {
          throw new Error(`decoder returned ${rate} Hz, expected ${SAMPLE_RATE} Hz`);
        }
        if (channels < 1 || channels > 2) {
          throw new Error(`decoder returned ${channels} channels`);
        }
        if (frames <= 0) throw new Error(`decoder returned ${frames} frames`);

        const samples = decoded.samples();
        if (samples.length !== frames * channels) {
          throw new Error(
            `interleaved length ${samples.length} != ${frames} × ${channels}`,
          );
        }
        for (let i = 0; i < samples.length; i += 1) {
          const value = samples[i];
          if (!Number.isFinite(value)) {
            throw new Error(`non-finite sample at index ${i}`);
          }
        }
        if (typeof entry.durationMs === "number") {
          const decodedMs = (frames / rate) * 1000;
          if (Math.abs(decodedMs - entry.durationMs) > 100) {
            throw new Error(
              `decoded ${decodedMs.toFixed(1)} ms vs registry ${entry.durationMs} ms`,
            );
          }
        }

        timings.push(decodeMs);
        framesTotal += frames;
        passed += 1;
      } finally {
        decoded.free();
      }
    } catch (error) {
      errors.push(`${id} — ${describeError(error)}`);
    }

    els.progress.value = index + 1;
    if ((index + 1) % 25 === 0 || index + 1 === entries.length) {
      els.allStatus.textContent =
        `Decoding ${index + 1}/${entries.length}… ` +
        `${passed} passed, ${errors.length} failed`;
      // Let the browser paint the progress line.
      await new Promise((resolve) => setTimeout(resolve, 0));
    }
  }

  const wallMs = performance.now() - wallStart;
  const seconds = framesTotal / SAMPLE_RATE;
  const totalDecodeMs = timings.reduce((sum, value) => sum + value, 0);

  // Registry-level problems count as failures too; surface them before the
  // headline so it cannot say "0 failed" while an error is listed.
  if (countMismatch) errors.unshift(countMismatch);

  setText(
    els.stAllClips,
    `${passed}/${entries.length} passed` +
      (errors.length ? ` · ${errors.length} FAILED` : " · 0 failed"),
  );
  els.stAllClips.className = errors.length ? "result-bad" : "result-ok";
  setText(els.stAllAudio, `${seconds.toFixed(2)} s`);
  setText(
    els.stAllDecode,
    timings.length
      ? `min ${timings.length ? Math.min(...timings).toFixed(2) : "—"} · ` +
          `median ${median(timings).toFixed(2)} · ` +
          `max ${Math.max(...timings).toFixed(2)} · ` +
          `mean ${(totalDecodeMs / timings.length).toFixed(2)}`
      : "—",
  );
  setText(
    els.stAllThroughput,
    timings.length
      ? `${msPerSecondOfAudio(totalDecodeMs, seconds).toFixed(2)} ms per second of audio ` +
          `(${(seconds / (totalDecodeMs / 1000)).toFixed(0)}× realtime)`
      : "—",
  );
  setText(
    els.stAllRate,
    ctx
      ? `${ctx.sampleRate} Hz (observed AudioContext.sampleRate)`
      : "AudioContext unavailable",
  );
  setText(els.stAllWall, `${(wallMs / 1000).toFixed(2)} s including fetches`);

  if (errors.length) {
    els.allStatus.textContent = `Done: ${passed}/${entries.length} decoded, ${errors.length} failure(s).`;
    els.allStatus.className = "error";
    els.errorsSummary.textContent = `Failures (${errors.length})`;
    els.errors.textContent = errors.join("\n");
    els.errorsWrap.hidden = false;
  } else {
    els.allStatus.textContent =
      `Done: ${entries.length}/${entries.length} decoded, zero failures.`;
    els.allStatus.className = "ok";
    els.errorsWrap.hidden = true;
  }

  state.busyAll = false;
  setBusy(false);
}

// --- Start ------------------------------------------------------------------

boot().catch((error) => {
  els.fatal.hidden = false;
  els.fatal.classList.add("fatal");
  setText(els.fatalMessage, describeError(error));
  setStatus(`Fatal: ${describeError(error)}`, "error");
  setBusy(true);
});
