#!/usr/bin/env node
/**
 * W-01 verification: the seeded voice API through the real wasm module.
 *
 *   node crates/rasp-wasm/verify.mjs              # build + verify
 *   node crates/rasp-wasm/verify.mjs --skip-build # reuse target/wasm-node-verify-w01
 *   node crates/rasp-wasm/verify.mjs --skip-native # skip the native-CLI cost check
 *
 * Plain Node, no npm dependencies. It builds `crates/rasp-wasm` for
 * wasm32-unknown-unknown, runs `wasm-bindgen --target nodejs` into
 * `target/wasm-node-verify-w01/` (gitignored), optimises with `wasm-opt -Oz`
 * when binaryen is present, then:
 *
 *   1. resolves 20 seed strings across the six D13 calibration voices,
 *   2. decodes each voice's `hello` clip and renders it twice through
 *      `render(bytes, seed)`,
 *   3. asserts 48 kHz decode, exact output length (within one sample of the
 *      decoded input), every sample finite, and that both renders are
 *      **byte-identical**,
 *   4. checks `describe` purity/determinism, the schema, and that junk bytes
 *      throw a typed `DecodeError:` string rather than trapping,
 *   5. reports wasm raw/gzip size before and after `wasm-opt -Oz`, decode and
 *      render ms per audio second,
 *   6. measures the same operating point through the native `rasp` CLI and
 *      asserts wasm render cost is at most 2× native (PLAN W-01).
 *
 * Exit code 1 on any failure; every failure names its seed and voice.
 */

import { spawnSync } from "node:child_process";
import { createRequire } from "node:module";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import zlib from "node:zlib";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(HERE, "../..");
const OUT_DIR = path.join(ROOT, "target", "wasm-node-verify-w01");
const RAW_WASM = path.join(
  ROOT,
  "target",
  "wasm32-unknown-unknown",
  "release",
  "rasp_wasm.wasm",
);
const EXPECTED_BINDGEN = "0.2.129";
const SAMPLE_RATE = 48_000;
/** The D13 curated calibration voices (6 of the 54). */
const TARGET_VOICES = [
  "am_onyx",
  "af_aoede",
  "bf_alice",
  "af_heart",
  "zf_xiaoni",
  "jm_kumo",
];
const SEEDS_REQUIRED = 20;
const DESCRIBE_SEEDS = ["npc-0", "npc-1", "park-42"];

const skipBuild = process.argv.includes("--skip-build");
const skipNative = process.argv.includes("--skip-native");
const failures = [];

function fail(message) {
  console.error(`FAIL ${message}`);
  failures.push(message);
}

function run(command, args, options = {}) {
  const result = spawnSync(command, args, {
    cwd: ROOT,
    stdio: options.capture ? "pipe" : "inherit",
    encoding: "utf8",
    maxBuffer: 64 * 1024 * 1024,
    ...options,
  });
  if (result.error) {
    throw new Error(`${command} could not start: ${result.error.message}`);
  }
  if (result.status !== 0) {
    const detail = options.capture
      ? `\n${result.stdout ?? ""}${result.stderr ?? ""}`
      : "";
    throw new Error(`${command} exited ${result.status}${detail}`);
  }
  return result.stdout ?? "";
}

function kib(bytes) {
  return (bytes / 1024).toFixed(1);
}

function gzipBytes(file) {
  const probe = spawnSync("gzip", ["-9", "-c", file], {
    maxBuffer: 64 * 1024 * 1024,
  });
  if (probe.status === 0 && probe.stdout) return probe.stdout.length;
  return zlib.gzipSync(fs.readFileSync(file), { level: 9 }).length;
}

function now() {
  return Number(process.hrtime.bigint()) / 1e6;
}

function percentile(sorted, p) {
  if (sorted.length === 0) return NaN;
  const index = Math.min(
    sorted.length - 1,
    Math.max(0, Math.ceil((p / 100) * sorted.length) - 1),
  );
  return sorted[index];
}

function sameBytes(a, b) {
  if (a.byteLength !== b.byteLength) return false;
  const left = Buffer.from(a.buffer, a.byteOffset, a.byteLength);
  const right = Buffer.from(b.buffer, b.byteOffset, b.byteLength);
  return left.equals(right);
}

// --- Build ------------------------------------------------------------------
// Keep in sync with scripts/build_wasm.sh and scripts/verify_wasm_decode.mjs:
// panic location strings bake in these remapped prefixes.
const REMAP_FLAGS = [
  `--remap-path-prefix=${os.homedir()}/.cargo=/cargo`,
  `--remap-path-prefix=${os.homedir()}/.rustup=/rustup`,
  `--remap-path-prefix=${ROOT}=/rasp`,
].join(" ");
const BUILD_ENV = {
  ...process.env,
  RUSTFLAGS: [process.env.RUSTFLAGS, REMAP_FLAGS].filter(Boolean).join(" "),
};

if (!skipBuild) {
  console.log("==> cargo build -p rasp-wasm --target wasm32-unknown-unknown --release");
  run(
    "cargo",
    ["build", "-p", "rasp-wasm", "--target", "wasm32-unknown-unknown", "--release"],
    { env: BUILD_ENV },
  );

  const bindgenVersion = run("wasm-bindgen", ["--version"], { capture: true })
    .trim()
    .split(/\s+/)
    .pop();
  if (bindgenVersion !== EXPECTED_BINDGEN) {
    throw new Error(
      `wasm-bindgen CLI ${bindgenVersion} != crate pin ${EXPECTED_BINDGEN}`,
    );
  }
  console.log(
    `    cargo artifact: ${fs.statSync(RAW_WASM).size} B raw ` +
      `(${kib(fs.statSync(RAW_WASM).size)} KiB, pre-bindgen)`,
  );

  fs.rmSync(OUT_DIR, { recursive: true, force: true });
  console.log(`==> wasm-bindgen --target nodejs -> ${path.relative(ROOT, OUT_DIR)}`);
  run("wasm-bindgen", [
    "--target",
    "nodejs",
    "--out-dir",
    OUT_DIR,
    "--out-name",
    "rasp",
    RAW_WASM,
  ]);
} else if (!fs.existsSync(path.join(OUT_DIR, "rasp.js"))) {
  throw new Error(
    `--skip-build needs ${path.relative(ROOT, OUT_DIR)}/rasp.js; run without it first`,
  );
}

// --- Size: raw/gzip before and after wasm-opt -Oz ---------------------------
const nodeWasmPath = path.join(OUT_DIR, "rasp_bg.wasm");
const bindgenRaw = fs.statSync(nodeWasmPath).size;
const bindgenGzip = gzipBytes(nodeWasmPath);
let finalRaw = bindgenRaw;
let finalGzip = bindgenGzip;
if (!skipBuild) {
  const optCheck = spawnSync("wasm-opt", ["--version"], { encoding: "utf8" });
  if (optCheck.status === 0) {
    const optimised = `${nodeWasmPath}.opt`;
    run("wasm-opt", ["-Oz", nodeWasmPath, "-o", optimised]);
    fs.renameSync(optimised, nodeWasmPath);
    finalRaw = fs.statSync(nodeWasmPath).size;
    finalGzip = gzipBytes(nodeWasmPath);
    console.log(
      `==> wasm-opt -Oz: ${bindgenRaw} B -> ${finalRaw} B raw · ` +
        `${bindgenGzip} B -> ${finalGzip} B gzip`,
    );
  } else {
    console.log("!! wasm-opt not installed; measuring the unoptimised wasm");
  }
} else {
  console.log("==> wasm-opt: skipped (--skip-build; reusing the artifact on disk)");
}

const require = createRequire(import.meta.url);
const wasm = require(path.join(OUT_DIR, "rasp.js"));
console.log(`    module ${wasm.version()}`);
console.log(
  `    wasm ${finalRaw} B raw / ${finalGzip} B gzip ` +
    `(${kib(finalRaw)} / ${kib(finalGzip)} KiB)` +
    (skipBuild ? "  [artifact on disk]" : ""),
);

// --- API surface + describe -------------------------------------------------
if (typeof wasm.variant_schema !== "function") fail("variant_schema is missing");
if (typeof wasm.describe !== "function") fail("describe is missing");
if (typeof wasm.render !== "function") fail("render is missing");
if (typeof wasm.decode !== "function") fail("decode (legacy D-04 export) is missing");

const schema = wasm.variant_schema();
if (schema !== 1) fail(`variant_schema() = ${schema}, expected 1`);

const described = [];
for (const seed of DESCRIBE_SEEDS) {
  const spec = wasm.describe(seed);
  described.push(spec);
  const again = wasm.describe(seed);
  if (JSON.stringify(spec) !== JSON.stringify(again)) {
    fail(`describe(${JSON.stringify(seed)}) is not deterministic`);
  }
  if (spec.seed !== seed) fail(`describe(${seed}).seed = ${spec.seed}`);
  if (spec.schema !== schema) fail(`describe(${seed}).schema = ${spec.schema}`);
  for (const field of ["voiceId", "archetype"]) {
    if (typeof spec[field] !== "string" || spec[field] === "") {
      fail(`describe(${seed}).${field} is not a non-empty string`);
    }
  }
  for (const field of ["pitchSt", "formantRatio", "tiltDbPerOct"]) {
    if (typeof spec[field] !== "number" || !Number.isFinite(spec[field])) {
      fail(`describe(${seed}).${field} is not a finite number`);
    }
  }
}
if (failures.length === 0) {
  console.log("\n==> describe(seed)");
  for (const spec of described) console.log(`    ${JSON.stringify(spec)}`);
}

// --- Registry + seed selection ---------------------------------------------
const registry = JSON.parse(
  fs.readFileSync(path.join(ROOT, "web", "registry.json"), "utf8"),
);
const seedForVoice = new Map(TARGET_VOICES.map((voice) => [voice, []]));
const used = new Set();
for (let index = 0; index < 200_000; index += 1) {
  const seed = `npc-${index}`;
  const voice = wasm.describe(seed).voiceId;
  const list = seedForVoice.get(voice);
  const wanted = TARGET_VOICES.indexOf(voice) < 2 ? 4 : 3;
  if (list && list.length < wanted && !used.has(seed)) {
    list.push(seed);
    used.add(seed);
  }
  if (
    TARGET_VOICES.reduce(
      (sum, id) => sum + seedForVoice.get(id).length,
      0,
    ) >= SEEDS_REQUIRED
  ) {
    break;
  }
}
const jobs = [];
for (const voice of TARGET_VOICES) {
  for (const seed of seedForVoice.get(voice)) {
    jobs.push({ seed, voice });
  }
}
if (jobs.length !== SEEDS_REQUIRED) {
  fail(`found ${jobs.length} seeds across six voices, expected ${SEEDS_REQUIRED}`);
}

// --- Typed errors -----------------------------------------------------------
for (const [label, action] of [
  [
    "render(junk)",
    () => wasm.render(new Uint8Array([0x00, 0x01, 0x02, 0x03]), "npc-0"),
  ],
]) {
  try {
    action();
    fail(`${label} returned instead of throwing`);
  } catch (error) {
    const text = typeof error === "string" ? error : String(error?.message ?? error);
    if (typeof error !== "string") {
      fail(`${label} threw a ${error?.name ?? typeof error}, not a coded string`);
    } else if (!text.startsWith("DecodeError: ")) {
      fail(`${label} error ${JSON.stringify(text)} lacks the DecodeError: prefix`);
    } else {
      console.log(`    errors: typed ${JSON.stringify(text.slice(0, 48))}… — no trap`);
    }
  }
}

// --- 20 renders -------------------------------------------------------------
const rows = [];
const renderTimes = [];
const decodeTimes = [];
let audioSeconds = 0;
let distinctVoices = new Set();
for (const job of jobs) {
  const entry = registry.files?.[job.voice]?.hello;
  if (!entry) {
    fail(`${job.voice}: registry has no hello clip`);
    continue;
  }
  const bytes = fs.readFileSync(path.join(ROOT, "web", entry.path));

  const decodeStart = now();
  let decoded;
  try {
    decoded = wasm.decode(bytes);
  } catch (error) {
    fail(`${job.seed}/${job.voice}: decode threw ${error?.message ?? error}`);
    continue;
  }
  const decodeMs = now() - decodeStart;
  decodeTimes.push(decodeMs);
  const rate = decoded.sample_rate();
  const channels = decoded.channels();
  const frames = decoded.length();
  const source = decoded.samples();
  decoded.free();
  if (rate !== SAMPLE_RATE) fail(`${job.voice}: decode rate ${rate} != 48000`);
  if (channels !== 1) fail(`${job.voice}: decode channels ${channels} != 1`);
  if (source.length !== frames) fail(`${job.voice}: source ${source.length} != ${frames}`);

  let first;
  let second;
  let renderMs;
  try {
    first = wasm.render(bytes, job.seed);
    const start = now();
    second = wasm.render(bytes, job.seed);
    renderMs = now() - start;
  } catch (error) {
    fail(`${job.seed}/${job.voice}: render threw ${error?.message ?? error}`);
    continue;
  }

  const spec = wasm.describe(job.seed);
  if (spec.voiceId !== job.voice) {
    fail(`${job.seed}: describe says ${spec.voiceId}, job expects ${job.voice}`);
  }
  distinctVoices.add(spec.voiceId);

  if (second.length !== frames) {
    fail(
      `${job.seed}/${job.voice}: render ${second.length} samples vs decoded ${frames}` +
        ` (delta ${second.length - frames})`,
    );
  }
  if (!sameBytes(first, second)) {
    fail(`${job.seed}/${job.voice}: two renders are not byte-identical`);
  }
  let peak = 0;
  let nonFinite = 0;
  for (let index = 0; index < second.length; index += 1) {
    const value = second[index];
    if (!Number.isFinite(value)) {
      nonFinite += 1;
      if (nonFinite === 1) fail(`${job.seed}/${job.voice}: non-finite at ${index}`);
    } else {
      peak = Math.max(peak, Math.abs(value));
    }
  }
  if (peak > 1.01) fail(`${job.seed}/${job.voice}: peak ${peak.toFixed(4)} > 1.01`);

  const seconds = second.length / SAMPLE_RATE;
  audioSeconds += seconds;
  renderTimes.push(renderMs);
  rows.push({
    seed: job.seed,
    voice: job.voice,
    archetype: spec.archetype,
    pitchSt: spec.pitchSt,
    formant: spec.formantRatio,
    tilt: spec.tiltDbPerOct,
    frames,
    seconds,
    renderMs,
    peak,
  });
}

// --- Report -----------------------------------------------------------------
if (rows.length > 0) {
  console.log("\n==> 20 seeded renders (6 voices)");
  console.log(
    "    seed     voice           archetype       pitch  formant  tilt   frames  durMs  renderMs  ms/s",
  );
  for (const row of rows) {
    console.log(
      `    ${row.seed.padEnd(8)} ${row.voice.padEnd(15)} ${row.archetype.padEnd(15)} ` +
        `${row.pitchSt.toFixed(2).padStart(6)} ${row.formant.toFixed(3).padStart(7)} ` +
        `${row.tilt.toFixed(2).padStart(5)} ${String(row.frames).padStart(7)} ` +
        `${(row.seconds * 1000).toFixed(1).padStart(6)} ` +
        `${row.renderMs.toFixed(2).padStart(8)} ` +
        `${(row.renderMs / row.seconds).toFixed(1).padStart(6)}`,
    );
  }
}
console.log(`\n    voices drawn: ${distinctVoices.size}/${TARGET_VOICES.length}`);
console.log(`    audio:        ${audioSeconds.toFixed(3)} s in ${rows.length} lines`);

const sortedRender = [...renderTimes].sort((a, b) => a - b);
const sortedDecode = [...decodeTimes].sort((a, b) => a - b);
const renderTotal = renderTimes.reduce((sum, value) => sum + value, 0);
const decodeTotal = decodeTimes.reduce((sum, value) => sum + value, 0);
const wasmTotalMsPerSec = audioSeconds > 0 ? renderTotal / audioSeconds : NaN;
const wasmDecodeMsPerSec = audioSeconds > 0 ? decodeTotal / audioSeconds : NaN;
// `render` decodes internally, so subtracting the measured decode cost gives
// the DSP share. Both numbers are reported; the native comparison uses DSP.
const wasmDspMsPerSec = Math.max(0, wasmTotalMsPerSec - wasmDecodeMsPerSec);
console.log(
  `    decode ms/s:  ${wasmDecodeMsPerSec.toFixed(2)}  ` +
    `(p50 ${percentile(sortedDecode, 50).toFixed(2)} ms/clip)`,
);
console.log(
  `    render ms/s:  ${wasmTotalMsPerSec.toFixed(2)} total, ` +
    `${wasmDspMsPerSec.toFixed(2)} DSP (decode excluded)  ` +
    `(p50 ${percentile(sortedRender, 50).toFixed(2)} ms/clip)`,
);

// --- Native CLI comparison (≤ 2×) ------------------------------------------
if (!skipNative) {
  const reference = jobs[0];
  const entry = registry.files[reference.voice].hello;
  const spec = wasm.describe(reference.seed);
  const outPath = path.join(
    fs.mkdtempSync(path.join(os.tmpdir(), "rasp-native-")),
    "reference.wav",
  );

  console.log("\n==> native CLI comparison (same clip, same operating point)");
  run("cargo", ["build", "--release", "-p", "rasp-cli"]);
  const cli = path.join(ROOT, "target", "release", "rasp");
  const stdout = run(
    cli,
    [
      "render",
      "--in",
      path.join(ROOT, "web", entry.path),
      "--pitch",
      String(spec.pitchSt),
      "--formant",
      String(spec.formantRatio),
      "--tilt",
      String(spec.tiltDbPerOct),
      "--headroom",
      "on",
      "--loudness",
      "on",
      "--out",
      outPath,
      "--json",
    ],
    { capture: true },
  );
  let native;
  try {
    native = JSON.parse(stdout);
  } catch (error) {
    fail(`native CLI produced non-JSON stdout: ${error.message}`);
  }
  if (native) {
    const nativeMsPerSec = native.renderMs / native.durationS;
    console.log(
      `    native: decode ${native.decodeMs.toFixed(2)} ms, ` +
        `render ${native.renderMs.toFixed(2)} ms / ${native.durationS.toFixed(3)} s ` +
        `= ${nativeMsPerSec.toFixed(2)} ms/s`,
    );
    console.log(
      `    wasm:   ${wasmDspMsPerSec.toFixed(2)} ms/s DSP ` +
        `= ${(wasmDspMsPerSec / nativeMsPerSec).toFixed(2)}× native`,
    );
    if (wasmDspMsPerSec > 2 * nativeMsPerSec) {
      fail(
        `wasm render ${wasmDspMsPerSec.toFixed(2)} ms/s > 2× native ` +
          `${nativeMsPerSec.toFixed(2)} ms/s`,
      );
    }
  }
} else {
  console.log("\n==> native CLI comparison: SKIP (--skip-native)");
}

// --- Verdict ----------------------------------------------------------------
if (failures.length > 0) {
  console.log(`\nFAILED: ${failures.length} failure(s)`);
  for (const message of failures) console.log(`  - ${message}`);
  process.exit(1);
}
console.log(
  `\nPASS: ${rows.length} seeded renders across ${distinctVoices.size} voices, ` +
    `byte-identical repeats, exact lengths, all finite.`,
);
console.log(
  `      wasm ${finalRaw} B raw / ${finalGzip} B gzip; ` +
    `render ${wasmDspMsPerSec.toFixed(2)} ms/s DSP.`,
);
