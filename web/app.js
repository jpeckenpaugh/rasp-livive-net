// RASP demo — seeded voices from the 54-voice Kokoro corpus.
//
// No framework, no build step, no CDN. The registry and `.weba` clips are
// same-origin, and the wasm engine is imported dynamically so a missing or
// broken engine degrades to "Engine unavailable" instead of a dead module.
//
// Layout of this file:
//   1. constants, DOM map, state, caches
//   2. formatting + status helpers
//   3. URL state
//   4. registry, filters and selects
//   5. knobs, presets and the D22 effective readout
//   6. seeds
//   7. decode / render caches and playback
//   8. the NPC grid
//   9. engine boot + app boot

const SAMPLE_RATE = 48_000;
const DEFAULT_SEED = "npc-123";
const NPC_COUNT = 12;
const PITCH_STEP = 0.05;
const FORMANT_STEP = 0.005;
const TILT_STEP = 0.25;

/** D12 preset points, from docs/presets.md §1. */
const PRESETS = Object.freeze({
  natural: { pitch: 0, formant: 1.0, tilt: 0 },
  "shift-up": { pitch: 7, formant: 1.1, tilt: 0.75 },
  "shift-down": { pitch: -3, formant: 0.88, tilt: 0 },
  bright: { pitch: 0, formant: 1.0, tilt: 1.5 },
  deep: { pitch: -4, formant: 0.85, tilt: 0 },
});

// Test/debug overrides: `?wasm=./wasm/nope.js` exercises the failure path
// without touching this file. They are also preserved in the share URL.
const bootParams = new URLSearchParams(location.search);
const ENGINE_MODULE = bootParams.get("wasm") || "./wasm/rasp.js";
const ENGINE_WASM = bootParams.get("wasmbin") || "./wasm/rasp_bg.wasm";

// Cache-busting for the wasm bundle. GitHub Pages serves assets with
// max-age=600, so a returning browser can pair a fresh app.js with an older
// rasp.js. The deploy writes version.json (commit SHA); append it to the
// engine and registry URLs so a deploy is atomic from the page's point of view.
let assetVersion = "";
function withVersion(url) {
  if (!assetVersion) return url;
  return `${url}${url.includes("?") ? "&" : "?"}v=${encodeURIComponent(assetVersion)}`;
}
async function readAssetVersion() {
  try {
    const response = await fetch("./version.json", { cache: "no-store" });
    if (!response.ok) return;
    const data = await response.json();
    assetVersion = String(data.version || "").slice(0, 12);
  } catch {
    /* local dev without version.json — no cache busting needed */
  }
}

const $ = (id) => document.getElementById(id);

const els = {
  engineState: $("engine-state"),
  engineSchema: $("engine-schema"),
  engineSize: $("engine-size"),
  engineRegistry: $("engine-registry"),

  voiceCount: $("voice-count"),
  languageChips: $("language-chips"),
  genderChips: $("gender-chips"),
  voiceSelect: $("voice-select"),
  phraseSelect: $("phrase-select"),
  voiceHint: $("voice-hint"),
  playOriginal: $("play-original"),

  resetKnobs: $("reset-knobs"),
  pitch: $("pitch"),
  pitchOut: $("pitch-out"),
  formant: $("formant"),
  formantOut: $("formant-out"),
  tilt: $("tilt"),
  tiltOut: $("tilt-out"),
  presets: Array.from(document.querySelectorAll(".preset")),
  effective: $("effective"),

  seedInput: $("seed-input"),
  derive: $("derive"),
  newSeed: $("new-seed"),
  seedSchema: $("seed-schema"),
  seedVoice: $("seed-voice"),
  seedArchetype: $("seed-archetype"),
  seedParams: $("seed-params"),
  seedSchemaValue: $("seed-schema-value"),

  playMode: $("play-mode"),
  playTransformed: $("play-transformed"),
  playOriginal2: $("play-original-2"),
  transportNote: $("transport-note"),

  generateNpcs: $("generate-npcs"),
  npcGrid: $("npc-grid"),

  status: $("status"),
  statusDetail: $("status-detail"),
  originalAudio: $("original-audio"),
};

const state = {
  registry: null,
  voicesById: new Map(),
  phrasesById: new Map(),
  languageValues: [],
  genderValues: [],
  filters: { languages: new Set(), genders: new Set() },
  api: null,
  engineError: null,
  registryError: null,
  voiceId: null,
  phraseId: null,
  seed: DEFAULT_SEED,
  knobs: { ...PRESETS.natural },
  audioContext: null,
  source: null,
  playSeq: 0,
  selectionSeq: 0,
  effectiveSeq: 0,
  effectiveTimer: 0,
};

/** Fetched `.weba` bytes, by registry path. */
const bytesCache = new Map();
/** Decoded clips (Float32Array kept), by `voice/phrase`. */
const clipCache = new Map();
/** Rendered PCM, by `params|voice|phrase|knobs` or `seed|seed|phrase`. */
const renderCache = new Map();
/** Effective points from effective_params(), by clip + knobs. */
const effectiveCache = new Map();

// --- 2. Formatting + status ------------------------------------------------

function clamp(value, low, high) {
  return Math.min(high, Math.max(low, value));
}

function roundTo(value, step) {
  return Math.round(value / step) * step;
}

function normaliseKnobs(knobs) {
  return {
    pitch: roundTo(clamp(Number.isFinite(knobs.pitch) ? knobs.pitch : 0, -12, 12), PITCH_STEP),
    formant: roundTo(
      clamp(Number.isFinite(knobs.formant) ? knobs.formant : 1, 0.75, 1.33),
      FORMANT_STEP,
    ),
    tilt: roundTo(clamp(Number.isFinite(knobs.tilt) ? knobs.tilt : 0, -6, 6), TILT_STEP),
  };
}

function errorText(error) {
  if (typeof error === "string") return error;
  if (error instanceof Error) return error.message || error.name;
  return String(error);
}

/** A compact URL-safe number: 4, 1.05, -0.75 (no trailing zeros). */
function formatNumber(value) {
  return String(Number(value.toFixed(3)));
}

function signed(value, digits = 2) {
  const fixed = value.toFixed(digits);
  return `${Number(fixed) > 0 ? "+" : ""}${fixed}`;
}

function formatKiB(bytes) {
  return `${(bytes / 1024).toFixed(1)} KiB`;
}

function setStatus(message, kind = "", detail = "") {
  els.status.textContent = message;
  if (kind) els.status.dataset.kind = kind;
  else delete els.status.dataset.kind;
  els.statusDetail.textContent = detail || "—";
}

function knobKey(knobs) {
  return `${formatNumber(knobs.pitch)}|${formatNumber(knobs.formant)}|${formatNumber(knobs.tilt)}`;
}

// --- 3. URL state ----------------------------------------------------------

function readBootState() {
  const number = (name, fallback) => {
    const raw = bootParams.get(name);
    if (raw === null || raw.trim() === "") return fallback;
    const value = Number(raw);
    return Number.isFinite(value) ? value : fallback;
  };
  return {
    voice: bootParams.get("voice"),
    phrase: bootParams.get("phrase"),
    seed: bootParams.get("seed") ?? DEFAULT_SEED,
    knobs: normaliseKnobs({
      pitch: number("pitch", 0),
      formant: number("formant", 1),
      tilt: number("tilt", 0),
    }),
  };
}

function updateUrl() {
  if (!state.registry || !state.voiceId || !state.phraseId) return;
  const params = new URLSearchParams();
  params.set("voice", state.voiceId);
  params.set("phrase", state.phraseId);
  params.set("seed", state.seed);
  params.set("pitch", formatNumber(state.knobs.pitch));
  params.set("formant", formatNumber(state.knobs.formant));
  params.set("tilt", formatNumber(state.knobs.tilt));
  for (const name of ["wasm", "wasmbin"]) {
    const value = bootParams.get(name);
    if (value) params.set(name, value);
  }
  const url = `${location.pathname}?${params.toString()}${location.hash}`;
  try {
    history.replaceState(null, "", url);
  } catch {
    // A sandboxed context may forbid replaceState; sharing is best-effort.
  }
}

// --- 4. Registry, filters and selects --------------------------------------

function visibleVoices() {
  if (!state.registry) return [];
  return state.registry.voices.filter(
    (voice) =>
      state.filters.languages.has(voice.language) && state.filters.genders.has(voice.gender),
  );
}

function makeChip(label, count, onClick) {
  const button = document.createElement("button");
  button.type = "button";
  button.className = "chip";
  button.setAttribute("aria-pressed", "false");
  button.append(document.createTextNode(label));
  if (typeof count === "number") {
    const span = document.createElement("span");
    span.className = "count";
    span.textContent = String(count);
    button.append(span);
  }
  button.addEventListener("click", onClick);
  return button;
}

function syncChips(group, container) {
  const selected = state.filters[group];
  const values = group === "languages" ? state.languageValues : state.genderValues;
  const all = selected.size === values.length;
  for (const chip of container.querySelectorAll(".chip")) {
    if (chip.dataset.all) chip.setAttribute("aria-pressed", String(all));
    else chip.setAttribute("aria-pressed", String(!all && selected.has(chip.dataset.value)));
  }
}

function buildChipGroup(group, container, values, voiceField) {
  container.replaceChildren();
  const allChip = makeChip("All", state.registry.voices.length, () => {
    state.filters[group] = new Set(values);
    syncChips(group, container);
    onFiltersChanged();
  });
  allChip.dataset.all = "1";
  container.append(allChip);

  for (const value of values) {
    const count = state.registry.voices.filter((voice) => voice[voiceField] === value).length;
    const chip = makeChip(value, count, () => toggleFilter(group, container, value));
    chip.dataset.value = value;
    container.append(chip);
  }
  syncChips(group, container);
}

function toggleFilter(group, container, value) {
  const values = group === "languages" ? state.languageValues : state.genderValues;
  const selected = state.filters[group];
  if (selected.size === values.length) {
    // "All" behaves like a starting point: the first individual click picks
    // exactly that value; later clicks add or remove.
    state.filters[group] = new Set([value]);
  } else if (selected.has(value)) {
    selected.delete(value);
    if (selected.size === 0) state.filters[group] = new Set(values);
  } else {
    selected.add(value);
  }
  syncChips(group, container);
  onFiltersChanged();
}

function onFiltersChanged() {
  rebuildVoiceSelect(state.voiceId);
  updateUrl();
  prefetchCurrent();
  scheduleEffective(0);
}

function buildFilters() {
  const voices = state.registry.voices;
  state.languageValues = state.registry.languages?.length
    ? state.registry.languages
    : [...new Set(voices.map((voice) => voice.language))];
  state.genderValues = [...new Set(voices.map((voice) => voice.gender))].sort();
  state.filters.languages = new Set(state.languageValues);
  state.filters.genders = new Set(state.genderValues);
  buildChipGroup("languages", els.languageChips, state.languageValues, "language");
  buildChipGroup("genders", els.genderChips, state.genderValues, "gender");
}

function rebuildVoiceSelect(preferred) {
  if (!state.registry) return;
  const voices = visibleVoices();
  const wanted = preferred || state.voiceId;
  els.voiceSelect.replaceChildren();
  for (const voice of voices) {
    const option = document.createElement("option");
    option.value = voice.id;
    option.textContent = `${voice.id} · ${voice.language} · ${voice.gender} · ${voice.grade}`;
    els.voiceSelect.append(option);
  }
  const chosen = voices.find((voice) => voice.id === wanted) || voices[0] || null;
  state.voiceId = chosen ? chosen.id : null;
  if (chosen) els.voiceSelect.value = chosen.id;
  els.voiceCount.textContent = `${voices.length} of ${state.registry.voices.length} voices`;
  updateVoiceHint();
  updateTransportNote();
  updateOriginalAvailability();
}

function fillPhrases() {
  const phrases = state.registry.phrases;
  const groups = state.registry.groups?.length
    ? state.registry.groups
    : [...new Set(phrases.map((phrase) => phrase.group))];
  els.phraseSelect.replaceChildren();
  for (const group of groups) {
    const items = phrases.filter((phrase) => phrase.group === group);
    if (items.length === 0) continue;
    const optgroup = document.createElement("optgroup");
    optgroup.label = group.charAt(0).toUpperCase() + group.slice(1);
    for (const phrase of items) {
      const option = document.createElement("option");
      option.value = phrase.id;
      option.textContent = `${phrase.text} (${phrase.id})`;
      optgroup.append(option);
    }
    els.phraseSelect.append(optgroup);
  }
}

function updateVoiceHint() {
  const voice = state.voicesById.get(state.voiceId);
  const phrase = state.phrasesById.get(state.phraseId);
  els.voiceHint.textContent =
    voice && phrase
      ? `${voice.id} · ${voice.language} · ${voice.gender} · grade ${voice.grade} — “${phrase.text}” (${phrase.id}, ${phrase.group})`
      : "—";
}

function updateTransportNote() {
  const voice = state.voicesById.get(state.voiceId);
  const phrase = state.phrasesById.get(state.phraseId);
  els.transportNote.textContent =
    voice && phrase ? `${voice.id} · ${phrase.id} — “${phrase.text}”` : "—";
}

function updateOriginalAvailability() {
  const ready = Boolean(state.registry && state.voiceId && state.phraseId);
  els.playOriginal.disabled = !ready;
  els.playOriginal2.disabled = !ready;
}

function onSelectionChanged() {
  updateVoiceHint();
  updateTransportNote();
  updateOriginalAvailability();
  updateUrl();
  prefetchCurrent();
  scheduleEffective(0);
}

function loadRegistry(boot) {
  return fetch(withVersion("./registry.json"), { cache: "no-cache" })
    .then((response) => {
      if (!response.ok) throw new Error(`HTTP ${response.status} for registry.json`);
      return response.json();
    })
    .then((registry) => {
      const voices = Array.isArray(registry.voices) ? registry.voices : [];
      const phrases = Array.isArray(registry.phrases) ? registry.phrases : [];
      if (voices.length === 0 || phrases.length === 0) {
        throw new Error("registry.json has no voices or phrases");
      }
      state.registry = registry;
      state.voicesById = new Map(voices.map((voice) => [voice.id, voice]));
      state.phrasesById = new Map(phrases.map((phrase) => [phrase.id, phrase]));

      const fallbackVoice =
        voices.find((voice) => voice.id === registry.defaultVoice)?.id ?? voices[0].id;
      state.voiceId = voices.some((voice) => voice.id === boot.voice) ? boot.voice : fallbackVoice;
      const fallbackPhrase = phrases.some((phrase) => phrase.id === "greet")
        ? "greet"
        : phrases[0].id;
      state.phraseId = phrases.some((phrase) => phrase.id === boot.phrase)
        ? boot.phrase
        : fallbackPhrase;

      buildFilters();
      rebuildVoiceSelect(state.voiceId);
      fillPhrases();
      els.phraseSelect.value = state.phraseId;

      const build = registry.build ?? {};
      const totals = build.totals ?? {};
      els.engineRegistry.textContent =
        `build ${build.id ?? "?"} · ${totals.voices ?? voices.length} voices · ` +
        `${totals.phrases ?? phrases.length} lines`;
      els.engineRegistry.title = `${totals.clips ?? "?"} clips · ${totals.bytes ?? "?"} bytes`;

      updateVoiceHint();
      updateTransportNote();
      updateOriginalAvailability();
      updateUrl();
    });
}

function registryUnavailable(error) {
  state.registryError = error;
  els.engineRegistry.textContent = "unavailable";
  els.voiceCount.textContent = "0 voices";
  setStatus(
    `Registry failed: ${errorText(error)}`,
    "error",
    "The page needs ./registry.json and ./audio/ beside it. Voice and line lists cannot load.",
  );
}

// --- 5. Knobs, presets and the effective readout ---------------------------

function writeKnobsToControls() {
  els.pitch.value = String(state.knobs.pitch);
  els.formant.value = String(state.knobs.formant);
  els.tilt.value = String(state.knobs.tilt);
  renderKnobReadouts();
  syncPresetChips();
}

function renderKnobReadouts() {
  const { pitch, formant, tilt } = state.knobs;
  els.pitchOut.textContent = `${signed(pitch)} st · ${Math.round(pitch * 100)} c`;
  els.formantOut.textContent = `×${formant.toFixed(3)} · ${signed((formant - 1) * 100, 1)}%`;
  els.tiltOut.textContent = `${signed(tilt)} dB/oct`;
}

function syncPresetChips() {
  const { pitch, formant, tilt } = state.knobs;
  for (const chip of els.presets) {
    const preset = PRESETS[chip.dataset.preset];
    const active =
      Math.abs(pitch - preset.pitch) < 1e-6 &&
      Math.abs(formant - preset.formant) < 1e-6 &&
      Math.abs(tilt - preset.tilt) < 1e-6;
    chip.setAttribute("aria-pressed", String(active));
  }
}

function setKnobs(knobs, { update = true, refresh = true } = {}) {
  state.knobs = normaliseKnobs(knobs);
  writeKnobsToControls();
  if (update) updateUrl();
  if (refresh) scheduleEffective();
}

function scheduleEffective(delay = 140) {
  clearTimeout(state.effectiveTimer);
  state.effectiveTimer = setTimeout(() => {
    void refreshEffective();
  }, delay);
}

function renderEffective(nominal, effective) {
  const weight = effective.headroomWeight;
  const differs =
    Math.abs(effective.pitchSt - nominal.pitch) > 5e-4 ||
    Math.abs(effective.formantRatio - nominal.formant) > 5e-4 ||
    Math.abs(effective.tiltDbPerOct - nominal.tilt) > 5e-4;
  els.effective.dataset.state = differs ? "scaled" : "same";
  if (!differs) {
    els.effective.textContent = `effective: matches requested · headroom w ${weight.toFixed(3)}`;
    return;
  }
  const noHeadroom = weight <= 0.001 && Math.abs(nominal.pitch) > 0;
  els.effective.textContent =
    `effective after headroom: ${signed(effective.pitchSt)} st · ` +
    `×${effective.formantRatio.toFixed(3)} · ` +
    `${signed(effective.tiltDbPerOct)} dB/oct · w ${weight.toFixed(3)}` +
    (noHeadroom ? " (no headroom)" : "");
}

async function refreshEffective() {
  if (!state.api) {
    els.effective.textContent = state.engineError
      ? "effective: engine unavailable"
      : "effective: waiting for the engine…";
    els.effective.dataset.state = state.engineError ? "unavailable" : "pending";
    return;
  }
  if (!state.voiceId || !state.phraseId) {
    els.effective.textContent = "effective: pick a voice and a line";
    els.effective.dataset.state = "pending";
    return;
  }
  const token = ++state.effectiveSeq;
  const knobs = { ...state.knobs };
  const key = `${state.voiceId}|${state.phraseId}|${knobKey(knobs)}`;
  try {
    const { clip } = await getClip(state.voiceId, state.phraseId);
    if (token !== state.effectiveSeq) return;
    let effective = effectiveCache.get(key);
    if (!effective) {
      effective = state.api.effectiveParams(clip.bytes, knobs.pitch, knobs.formant, knobs.tilt);
      effectiveCache.set(key, effective);
    }
    if (token !== state.effectiveSeq) return;
    renderEffective(knobs, effective);
  } catch (error) {
    if (token === state.effectiveSeq) {
      els.effective.textContent = `effective: ${errorText(error)}`;
      els.effective.dataset.state = "error";
    }
  }
}

// --- 6. Seeds --------------------------------------------------------------

function randomSeed() {
  const alphabet = "abcdefghijklmnopqrstuvwxyz0123456789";
  const bytes = new Uint8Array(6);
  try {
    crypto.getRandomValues(bytes);
  } catch {
    for (let index = 0; index < bytes.length; index += 1) {
      bytes[index] = Math.floor(Math.random() * 256);
    }
  }
  let suffix = "";
  for (const byte of bytes) suffix += alphabet[byte % alphabet.length];
  return `npc-${suffix}`;
}

function describeSeed(seed) {
  if (!state.api) return null;
  try {
    return state.api.describe(seed);
  } catch {
    return null;
  }
}

function updateSeedReadout(spec) {
  const voice = spec ? state.voicesById.get(spec.voiceId) : null;
  els.seedVoice.textContent = spec
    ? voice
      ? `${spec.voiceId} · ${voice.language} · ${voice.gender}`
      : spec.voiceId
    : "—";
  els.seedArchetype.textContent = spec ? spec.archetype : "—";
  els.seedParams.textContent = spec
    ? `${signed(spec.pitchSt)} st · ×${spec.formantRatio.toFixed(3)} · ` +
      `${signed(spec.tiltDbPerOct)} dB/oct`
    : "—";
  els.seedSchemaValue.textContent = spec ? `v${spec.schema}` : "—";
  els.seedSchema.textContent = spec ? `schema v${spec.schema}` : "schema —";
}

function derive() {
  if (!state.api) {
    setStatus(
      "Engine unavailable — cannot derive a voice.",
      "error",
      "The seed field is still editable; Play original reads the same-origin .webm clip.",
    );
    return null;
  }
  const seed = els.seedInput.value;
  const spec = describeSeed(seed);
  if (!spec) {
    setStatus(`Could not describe seed ${JSON.stringify(seed)}.`, "error");
    return null;
  }
  state.seed = seed;
  // The derived voice may be outside the current filters; show the whole cast
  // rather than silently selecting a different voice.
  state.filters.languages = new Set(state.languageValues);
  state.filters.genders = new Set(state.genderValues);
  syncChips("languages", els.languageChips);
  syncChips("genders", els.genderChips);
  rebuildVoiceSelect(spec.voiceId);
  setKnobs(
    { pitch: spec.pitchSt, formant: spec.formantRatio, tilt: spec.tiltDbPerOct },
    { update: false, refresh: false },
  );
  updateSeedReadout(spec);
  updateVoiceHint();
  updateTransportNote();
  updateUrl();
  prefetchCurrent();
  scheduleEffective(0);
  setStatus(
    `Derived ${JSON.stringify(seed)} → ${spec.voiceId} · ${spec.archetype}`,
    "ok",
    `schema v${spec.schema} · requested ${signed(spec.pitchSt)} st · ` +
      `×${spec.formantRatio.toFixed(3)} · ${signed(spec.tiltDbPerOct)} dB/oct`,
  );
  return spec;
}

function newSeed() {
  els.seedInput.value = randomSeed();
  derive();
}

// --- 7. Decode / render caches and playback --------------------------------

function clipEntry(voiceId, phraseId) {
  return state.registry?.files?.[voiceId]?.[phraseId] ?? null;
}

function fetchBytes(path) {
  if (bytesCache.has(path)) return bytesCache.get(path);
  const promise = (async () => {
    const url = new URL(path, location.href).href;
    const response = await fetch(url, { cache: "no-cache" });
    if (!response.ok) throw new Error(`HTTP ${response.status} for ${path}`);
    return new Uint8Array(await response.arrayBuffer());
  })();
  bytesCache.set(path, promise);
  promise.catch(() => bytesCache.delete(path));
  return promise;
}

async function getClip(voiceId, phraseId) {
  const key = `${voiceId}/${phraseId}`;
  if (clipCache.has(key)) {
    return { clip: await clipCache.get(key), hit: true };
  }
  const promise = (async () => {
    const entry = clipEntry(voiceId, phraseId);
    if (!entry) throw new Error(`registry has no clip for ${voiceId}/${phraseId}`);
    if (!state.api) throw new Error("engine unavailable");
    const bytes = await fetchBytes(entry.path);
    const started = performance.now();
    const decoded = state.api.decode(bytes);
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
    const decodeMs = performance.now() - started;
    if (rate !== SAMPLE_RATE) throw new Error(`decoder returned ${rate} Hz, expected ${SAMPLE_RATE}`);
    if (frames <= 0) throw new Error(`decoder returned ${frames} frames`);
    if (samples.length !== frames * channels) {
      throw new Error(`decoded ${samples.length} samples != ${frames} × ${channels}`);
    }
    return {
      key,
      voiceId,
      phraseId,
      entry,
      bytes,
      samples,
      frames,
      channels,
      rate,
      decodeMs,
    };
  })();
  clipCache.set(key, promise);
  promise.catch(() => clipCache.delete(key));
  return { clip: await promise, hit: false };
}

function prefetchCurrent() {
  if (!state.api || !state.voiceId || !state.phraseId) return;
  const seq = ++state.selectionSeq;
  const voiceId = state.voiceId;
  const phraseId = state.phraseId;
  getClip(voiceId, phraseId)
    .then(({ clip, hit }) => {
      if (seq !== state.selectionSeq) return;
      els.playMode.textContent = `ready · ${(clip.frames / SAMPLE_RATE).toFixed(2)} s`;
      els.playMode.title =
        `${clip.frames} frames · decode ${hit ? "hit" : "miss"} · ` +
        `${clip.decodeMs.toFixed(1)} ms`;
    })
    .catch(() => {
      // The play path reports the typed error; a prefetch stays quiet.
    });
}

function renderForParams(voiceId, phraseId, clip, knobs) {
  const key = `params|${voiceId}|${phraseId}|${knobKey(knobs)}`;
  if (renderCache.has(key)) return { pcm: renderCache.get(key), hit: true, ms: 0 };
  const started = performance.now();
  const pcm = state.api.renderParams(clip.bytes, knobs.pitch, knobs.formant, knobs.tilt);
  const ms = performance.now() - started;
  renderCache.set(key, pcm);
  return { pcm, hit: false, ms };
}

function renderForSeed(seed, phraseId, clip) {
  const key = `seed|${seed}|${phraseId}`;
  if (renderCache.has(key)) return { pcm: renderCache.get(key), hit: true, ms: 0 };
  const started = performance.now();
  const pcm = state.api.render(clip.bytes, seed);
  const ms = performance.now() - started;
  renderCache.set(key, pcm);
  return { pcm, hit: false, ms };
}

function ensureAudioContext() {
  if (state.audioContext) return state.audioContext;
  const Ctor = window.AudioContext || window.webkitAudioContext;
  if (!Ctor) throw new Error("WebAudio is not available in this browser");
  try {
    state.audioContext = new Ctor({ sampleRate: SAMPLE_RATE, latencyHint: "interactive" });
  } catch {
    state.audioContext = new Ctor();
  }
  return state.audioContext;
}

/**
 * Create/resume the AudioContext inside the user gesture, before any await.
 * Called only from click/key handlers; the page never creates a context at
 * boot.
 */
function primeAudio() {
  if (!state.api) return;
  try {
    const context = ensureAudioContext();
    if (context.state === "suspended") {
      Promise.resolve(context.resume()).catch(() => {
        // A later gesture can resume it; playback still reports failures.
      });
    }
  } catch {
    // playSamples reports an unavailable AudioContext with the typed message.
  }
}

function stopTransformed() {
  if (!state.source) return;
  try {
    state.source.stop();
  } catch {
    // already stopped
  }
  try {
    state.source.disconnect();
  } catch {
    // already detached
  }
  state.source = null;
}

/** Schedule PCM at 48 kHz. The browser resamples to the device rate. */
function playSamples(pcm) {
  const context = ensureAudioContext();
  if (context.state === "suspended") {
    Promise.resolve(context.resume()).catch(() => {
      // The buffer is still scheduled; an explicit gesture will start it.
    });
  }
  // A/B is one voice at a time: stop the <audio> original before the render.
  els.originalAudio.pause();
  stopTransformed();
  const buffer = context.createBuffer(1, pcm.length, SAMPLE_RATE);
  if (typeof buffer.copyToChannel === "function") buffer.copyToChannel(pcm, 0);
  else buffer.getChannelData(0).set(pcm);
  const source = context.createBufferSource();
  source.buffer = buffer;
  source.connect(context.destination);
  source.onended = () => {
    try {
      source.disconnect();
    } catch {
      // already detached
    }
    if (state.source === source) state.source = null;
  };
  source.start();
  state.source = source;
}

function formatRenderDetail({ clip, pcm, decodeHit, renderHit, renderMs }) {
  return [
    `render ${renderMs.toFixed(1)} ms (${renderHit ? "hit" : "miss"})`,
    `decoded ${clip.frames.toLocaleString()} frames`,
    `buffer ${pcm.length.toLocaleString()} frames`,
    `decode ${decodeHit ? "hit" : "miss"}`,
  ].join(" · ");
}

async function playTransformed() {
  if (!state.api) {
    setStatus(
      "Engine unavailable — nothing to render.",
      "error",
      "Voice and line browsing still work; Play original reads the same-origin .weba.",
    );
    return;
  }
  if (!state.voiceId || !state.phraseId) {
    setStatus("Pick a voice and a line first.", "error");
    return;
  }
  primeAudio();
  const token = ++state.playSeq;
  const voiceId = state.voiceId;
  const phraseId = state.phraseId;
  const knobs = { ...state.knobs };
  els.playTransformed.disabled = true;
  try {
    setStatus(`Rendering ${voiceId} / ${phraseId}…`, "", "wasm → AudioBufferSourceNode");
    const { clip, hit: decodeHit } = await getClip(voiceId, phraseId);
    const { pcm, hit: renderHit, ms: renderMs } = renderForParams(
      voiceId,
      phraseId,
      clip,
      knobs,
    );
    if (token !== state.playSeq) return;
    playSamples(pcm);
    const mismatch = pcm.length !== clip.frames;
    const detail = formatRenderDetail({ clip, pcm, decodeHit, renderHit, renderMs });
    setStatus(
      mismatch
        ? `Length mismatch: buffer ${pcm.length} vs decoded ${clip.frames} frames`
        : `Playing transformed · ${voiceId} / ${phraseId}`,
      mismatch ? "error" : "ok",
      mismatch ? `${detail} · LENGTH MISMATCH` : detail,
    );
  } catch (error) {
    if (token === state.playSeq) {
      setStatus(
        `Transformed failed: ${errorText(error)}`,
        "error",
        "Typed error from the wasm boundary; the rest of the page still works.",
      );
    }
  } finally {
    if (token === state.playSeq) els.playTransformed.disabled = false;
  }
}

function playOriginal() {
  if (!state.registry || !state.voiceId || !state.phraseId) {
    setStatus("Pick a voice and a line first.", "error");
    return;
  }
  const entry = clipEntry(state.voiceId, state.phraseId);
  if (!entry) {
    setStatus(`No registry entry for ${state.voiceId}/${state.phraseId}.`, "error");
    return;
  }
  const audio = els.originalAudio;
  stopTransformed();
  audio.pause();
  audio.src = new URL(entry.path, location.href).href;
  audio.load();
  const play = audio.play();
  const clipped = clipCache.has(`${state.voiceId}/${state.phraseId}`);
  setStatus(
    `Playing original · ${state.voiceId} / ${state.phraseId}`,
    "ok",
    `source ${entry.path} · ${entry.durationMs} ms · ${entry.bytes.toLocaleString()} B · ` +
      `decode ${clipped ? "hit" : "miss"}`,
  );
  if (play && typeof play.catch === "function") {
    play.catch((error) => {
      if (error?.name === "NotAllowedError") {
        setStatus(
          "Original is ready — the browser blocked autoplay until a click.",
          "",
          `source ${entry.path}`,
        );
      } else {
        setStatus(`Original failed: ${errorText(error)}`, "error", entry.path);
      }
    });
  }
}

async function playSeed(seed, button) {
  if (!state.api) {
    setStatus("Engine unavailable — cannot render that seed.", "error");
    return;
  }
  if (!state.phraseId) {
    setStatus("Pick a line first.", "error");
    return;
  }
  primeAudio();
  const token = ++state.playSeq;
  const phraseId = state.phraseId;
  if (button) button.disabled = true;
  try {
    const spec = state.api.describe(seed);
    const { clip, hit: decodeHit } = await getClip(spec.voiceId, phraseId);
    const { pcm, hit: renderHit, ms: renderMs } = renderForSeed(seed, phraseId, clip);
    if (token !== state.playSeq) return;
    playSamples(pcm);
    const detail = formatRenderDetail({ clip, pcm, decodeHit, renderHit, renderMs });
    const mismatch = pcm.length !== clip.frames;
    setStatus(
      mismatch
        ? `Length mismatch: buffer ${pcm.length} vs decoded ${clip.frames} frames`
        : `Playing seed ${seed} · ${spec.voiceId} · ${spec.archetype} / ${phraseId}`,
      mismatch ? "error" : "ok",
      `${detail} · seed params ${signed(spec.pitchSt)} st · ` +
        `×${spec.formantRatio.toFixed(3)} · ${signed(spec.tiltDbPerOct)} dB/oct`,
    );
  } catch (error) {
    if (token === state.playSeq) {
      setStatus(`Seed ${seed} failed: ${errorText(error)}`, "error");
    }
  } finally {
    if (button) button.disabled = false;
  }
}

// --- 8. NPC grid -----------------------------------------------------------

function cardLine(label, value, className) {
  const node = document.createElement("span");
  node.className = className;
  node.textContent = value;
  node.setAttribute("aria-label", `${label}: ${value}`);
  return node;
}

function generateNpcs() {
  if (!state.api) {
    setStatus(
      "Engine unavailable — cannot generate a cast.",
      "error",
      "The seed generator lives in the wasm module.",
    );
    return;
  }
  if (!state.phraseId) {
    setStatus("Pick a line first — the cast plays the selected line.", "error");
    return;
  }
  const seeds = Array.from({ length: NPC_COUNT }, () => randomSeed());
  els.npcGrid.replaceChildren();
  for (const seed of seeds) {
    const spec = state.api.describe(seed);
    const voice = state.voicesById.get(spec.voiceId);
    const card = document.createElement("article");
    card.className = "npc-card";

    const code = document.createElement("code");
    code.textContent = seed;
    const voiceLine = cardLine(
      "Base voice",
      voice ? `${spec.voiceId} · ${voice.language} · ${voice.gender}` : spec.voiceId,
      "npc-voice",
    );
    const archetype = cardLine("Archetype", spec.archetype, "npc-archetype");
    const params = cardLine(
      "Requested parameters",
      `${signed(spec.pitchSt)} st · ×${spec.formantRatio.toFixed(3)} · ` +
        `${signed(spec.tiltDbPerOct)} dB/oct`,
      "npc-params",
    );

    const button = document.createElement("button");
    button.type = "button";
    button.textContent = "Play line";
    button.addEventListener("click", () => {
      void playSeed(seed, button);
    });

    card.append(code, voiceLine, archetype, params, button);
    els.npcGrid.append(card);
  }
  setStatus(
    `Generated ${NPC_COUNT} NPCs`,
    "ok",
    `line ${state.phraseId} · seeds ${seeds[0]} … ${seeds[seeds.length - 1]}`,
  );
}

// --- 9. Engine + boot ------------------------------------------------------

function enableEngineControls(enabled) {
  els.playTransformed.disabled = !enabled;
  els.derive.disabled = !enabled;
  els.generateNpcs.disabled = !enabled;
  updateOriginalAvailability();
}

function reportWasmSize() {
  fetch(new URL(ENGINE_WASM, location.href), { cache: "force-cache" })
    .then((response) => {
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      return response.arrayBuffer();
    })
    .then(async (buffer) => {
      const bytes = buffer.byteLength;
      let gzip = null;
      if (typeof CompressionStream === "function") {
        try {
          const stream = new Blob([buffer]).stream().pipeThrough(new CompressionStream("gzip"));
          gzip = (await new Response(stream).arrayBuffer()).byteLength;
        } catch {
          gzip = null;
        }
      }
      els.engineSize.textContent =
        gzip === null
          ? `${formatKiB(bytes)} (gzip n/a)`
          : `${formatKiB(bytes)} · ${formatKiB(gzip)} gzip`;
      els.engineSize.title = `${bytes.toLocaleString()} B raw` + (gzip ? ` · ${gzip} B gzip` : "");
    })
    .catch(() => {
      els.engineSize.textContent = "size unavailable";
    });
}

function engineUnavailable(error) {
  state.api = null;
  state.engineError = error;
  els.engineState.textContent = "Unavailable";
  els.engineState.dataset.kind = "bad";
  els.engineSchema.textContent = "—";
  els.engineSize.textContent = "—";
  els.playMode.textContent = "unavailable";
  enableEngineControls(false);
  els.effective.textContent = "effective: engine unavailable";
  els.effective.dataset.state = "unavailable";
  updateSeedReadout(null);
  setStatus(
    `Engine unavailable — ${errorText(error)}`,
    "error",
    "Voice and line browsing still work; Play original reads the same-origin .weba.",
  );
}

function engineReady(module) {
  state.api = {
    describe: module.describe,
    render: module.render,
    renderParams: module.render_params,
    effectiveParams: module.effective_params,
    decode: module.decode,
    schema: module.variant_schema(),
    version: typeof module.version === "function" ? module.version() : "rasp-wasm",
  };
  els.engineState.textContent = "Ready";
  els.engineState.dataset.kind = "ok";
  els.engineSchema.textContent = `v${state.api.schema}`;
  els.playMode.textContent = "ready";
  enableEngineControls(true);
  updateSeedReadout(describeSeed(state.seed));
  if (state.registry) {
    setStatus(
      `Engine ready · ${state.api.version}`,
      "ok",
      "Space plays transformed, O plays the original, N makes a new seed.",
    );
  } else {
    setStatus(
      `Engine ready, but the registry failed: ${errorText(state.registryError)}`,
      "error",
      "The engine's schema and size are real; there are no voices to select.",
    );
  }
  prefetchCurrent();
  scheduleEffective(0);
  reportWasmSize();
}

async function loadEngine() {
  setStatus("Loading the wasm engine…", "", `import ${ENGINE_MODULE}`);
  try {
    await readAssetVersion();
    const module = await import(withVersion(new URL(ENGINE_MODULE, location.href).href));
    if (typeof module.default !== "function") {
      throw new Error("wasm glue has no default init export");
    }
    // Object form: wasm-bindgen warns on a bare URL argument.
    await module.default({ module_or_path: withVersion(new URL(ENGINE_WASM, location.href).href) });
    for (const name of [
      "describe",
      "render",
      "render_params",
      "effective_params",
      "decode",
      "variant_schema",
    ]) {
      if (typeof module[name] !== "function") {
        throw new Error(`wasm module lacks ${name}()`);
      }
    }
    engineReady(module);
  } catch (error) {
    engineUnavailable(error);
  }
}

function isTypingTarget(target) {
  return (
    target instanceof Element &&
    target.closest("input, select, textarea, button, a, summary, audio, [contenteditable='true']") !==
      null
  );
}

function bindEvents() {
  els.voiceSelect.addEventListener("change", () => {
    state.voiceId = els.voiceSelect.value;
    onSelectionChanged();
  });
  els.phraseSelect.addEventListener("change", () => {
    state.phraseId = els.phraseSelect.value;
    onSelectionChanged();
  });

  els.pitch.addEventListener("input", () => {
    setKnobs({ ...state.knobs, pitch: Number(els.pitch.value) });
  });
  els.formant.addEventListener("input", () => {
    setKnobs({ ...state.knobs, formant: Number(els.formant.value) });
  });
  els.tilt.addEventListener("input", () => {
    setKnobs({ ...state.knobs, tilt: Number(els.tilt.value) });
  });
  els.resetKnobs.addEventListener("click", () => {
    setKnobs(PRESETS.natural);
    setStatus("Knobs reset to natural.", "", "0 st · ×1.000 · 0 dB/oct");
  });
  for (const chip of els.presets) {
    chip.addEventListener("click", () => {
      const preset = PRESETS[chip.dataset.preset];
      if (!preset) return;
      setKnobs(preset);
      setStatus(`Preset: ${chip.dataset.preset}`, "", `${signed(preset.pitch)} st · ×${preset.formant.toFixed(3)} · ${signed(preset.tilt)} dB/oct`);
    });
  }

  els.seedInput.addEventListener("input", () => {
    state.seed = els.seedInput.value;
    updateUrl();
  });
  els.seedInput.addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      derive();
    }
  });
  els.derive.addEventListener("click", () => {
    derive();
  });
  els.newSeed.addEventListener("click", () => {
    newSeed();
  });

  const onPlayTransformed = () => {
    void playTransformed();
  };
  els.playTransformed.addEventListener("click", onPlayTransformed);
  els.playOriginal.addEventListener("click", playOriginal);
  els.playOriginal2.addEventListener("click", playOriginal);
  els.originalAudio.addEventListener("error", () => {
    if (els.originalAudio.currentSrc) {
      setStatus("Original failed to load.", "error", els.originalAudio.currentSrc);
    }
  });

  els.generateNpcs.addEventListener("click", generateNpcs);

  window.addEventListener("keydown", (event) => {
    if (event.defaultPrevented || event.metaKey || event.ctrlKey || event.altKey) return;
    if (isTypingTarget(event.target)) return;
    if (event.key === " " || event.code === "Space") {
      event.preventDefault();
      primeAudio();
      void playTransformed();
    } else if (event.key === "o" || event.key === "O") {
      playOriginal();
    } else if (event.key === "n" || event.key === "N") {
      newSeed();
    }
  });

  window.addEventListener("error", (event) => {
    if (!event.error) return;
    setStatus(
      `Unexpected error: ${errorText(event.error)}`,
      "error",
      "The demo keeps running; reload if a control stops responding.",
    );
  });
  window.addEventListener("unhandledrejection", (event) => {
    event.preventDefault();
    setStatus(
      `Unexpected error: ${errorText(event.reason)}`,
      "error",
      "The demo keeps running; reload if a control stops responding.",
    );
  });
}

async function boot() {
  const bootState = readBootState();
  state.seed = bootState.seed;
  state.knobs = bootState.knobs;
  els.seedInput.value = state.seed;
  writeKnobsToControls();
  setStatus("Loading the registry…", "", "fetching ./registry.json");

  try {
    await loadRegistry(bootState);
  } catch (error) {
    registryUnavailable(error);
  }
  await loadEngine();
}

bindEvents();
boot().catch((error) => {
  setStatus(
    `The demo could not start: ${errorText(error)}`,
    "error",
    "Reload the page. If it repeats, open the browser console for the failing resource.",
  );
});
