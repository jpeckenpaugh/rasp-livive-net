#!/usr/bin/env node
/**
 * D-04 verification: decode the whole vendored corpus through the real wasm
 * module and check it against the committed registry and the generated fixture
 * manifest.
 *
 *   node scripts/verify_wasm_decode.mjs              # build + verify
 *   node scripts/verify_wasm_decode.mjs --skip-build # reuse target/wasm-node-verify
 *
 * Plain Node, no npm dependencies. It builds `crates/rasp-wasm` for
 * wasm32-unknown-unknown, runs `wasm-bindgen --target nodejs` into
 * `target/wasm-node-verify/` (gitignored), then:
 *
 *   1. decodes all 1404 clips referenced by `web/registry.json`,
 *   2. asserts 48 kHz, one channel, non-empty output, no NaN/Inf, peak <= 1.01,
 *      and a duration consistent with the registry's `durationMs` (a display
 *      hint, not the sample-count oracle — see `crates/rasp-opus/README.md`),
 *   3. asserts the 12 fixture sample counts in `fixtures/manifest.json` match
 *      exactly (the manifest is generated, so a missing manifest is a printed
 *      SKIP, not a silent pass),
 *   4. asserts malformed input surfaces as a typed `RaspDecodeError`, never a
 *      trap,
 *   5. reports wasm raw/gzip size and per-clip wall time.
 *
 * Exit code 1 on any failure. All failures are printed with their clip id.
 */

import { spawnSync } from "node:child_process";
import { createRequire } from "node:module";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import zlib from "node:zlib";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(HERE, "..");
const OUT_DIR = path.join(ROOT, "target", "wasm-node-verify");
const RAW_WASM = path.join(
  ROOT,
  "target",
  "wasm32-unknown-unknown",
  "release",
  "rasp_wasm.wasm",
);
const EXPECTED_BINDGEN = "0.2.129";
const SAMPLE_RATE = 48_000;
const EXPECTED_CLIPS = 1404;
// Decoded length vs the registry's rounded display duration. D-02 measured
// -7.1..-27.2 ms on the reference set; 100 ms catches gross corruption without
// re-litigating framing (which `cargo test -p rasp-opus` owns).
const DURATION_TOLERANCE_MS = 100;

const skipBuild = process.argv.includes("--skip-build");

function fail(message) {
  console.error(`FAIL ${message}`);
  failures.push(message);
}
const failures = [];

function run(command, args, options = {}) {
  const result = spawnSync(command, args, {
    cwd: ROOT,
    stdio: options.capture ? "pipe" : "inherit",
    encoding: "utf8",
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

function percentile(sorted, p) {
  if (sorted.length === 0) return NaN;
  const index = Math.min(
    sorted.length - 1,
    Math.max(0, Math.ceil((p / 100) * sorted.length) - 1),
  );
  return sorted[index];
}

// --- Build ------------------------------------------------------------------
// Keep this in sync with scripts/build_wasm.sh: the remapped paths are baked
// into panic location strings, and the byte-identity check below depends on
// both builds using the same flags.
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
    [
      "build",
      "-p",
      "rasp-wasm",
      "--target",
      "wasm32-unknown-unknown",
      "--release",
    ],
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

// Verify the optimised artifact, not a fatter one that never ships. The wasm
// binary is target-independent (wasm-bindgen's target only changes the JS
// glue), so this should be byte-identical to `web/wasm/rasp_bg.wasm`.
const nodeWasmPath = path.join(OUT_DIR, "rasp_bg.wasm");
const optCheck = spawnSync("wasm-opt", ["--version"], { encoding: "utf8" });
if (!skipBuild && optCheck.status === 0) {
  const before = fs.statSync(nodeWasmPath).size;
  const optimised = `${nodeWasmPath}.opt`;
  run("wasm-opt", ["-Oz", nodeWasmPath, "-o", optimised]);
  fs.renameSync(optimised, nodeWasmPath);
  console.log(
    `    wasm-opt -Oz: ${before} B -> ${fs.statSync(nodeWasmPath).size} B`,
  );
} else if (optCheck.status !== 0) {
  console.log(
    "    wasm-opt: not installed; verifying the unoptimised wasm (install binaryen to match the shipped build)",
  );
} else {
  console.log("    wasm-opt: skipped (--skip-build; reusing the optimised artifact)");
}

const require = createRequire(import.meta.url);
const modulePath = path.join(OUT_DIR, "rasp.js");
const wasm = require(modulePath);
const wasmBytes = fs.readFileSync(nodeWasmPath);
const rawBytes = wasmBytes.length;
// Match `scripts/build_wasm.sh` (system `gzip -9`) so the two reports agree;
// zlib is the fallback when gzip is absent.
const gzipProbe = spawnSync("gzip", ["-9", "-c", nodeWasmPath], {
  maxBuffer: 64 * 1024 * 1024,
});
const gzipBytes =
  gzipProbe.status === 0 && gzipProbe.stdout
    ? gzipProbe.stdout.length
    : zlib.gzipSync(wasmBytes, { level: 9 }).length;

console.log(
  `    module ${wasm.version()}  ·  wasm ${rawBytes} B raw / ${gzipBytes} B gzip ` +
    `(${kib(rawBytes)} / ${kib(gzipBytes)} KiB)`,
);

const shippedWasm = path.join(ROOT, "web", "wasm", "rasp_bg.wasm");
if (fs.existsSync(shippedWasm)) {
  const shipped = fs.readFileSync(shippedWasm);
  if (shipped.equals(wasmBytes)) {
    console.log("    shipped wasm: web/wasm/rasp_bg.wasm is byte-identical");
  } else {
    console.log(
      `    shipped wasm: DIFFERS from web/wasm/rasp_bg.wasm ` +
        `(${shipped.length} B); run bash scripts/build_wasm.sh`,
    );
  }
}

// --- Registry ---------------------------------------------------------------
const registry = JSON.parse(
  fs.readFileSync(path.join(ROOT, "web", "registry.json"), "utf8"),
);
const clips = [];
for (const [voice, phrases] of Object.entries(registry.files)) {
  for (const [phrase, entry] of Object.entries(phrases)) {
    clips.push({ id: `${voice}/${phrase}`, voice, phrase, ...entry });
  }
}
clips.sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));

const expectedTotal = registry.build?.totals?.clips;
if (expectedTotal !== undefined && expectedTotal !== clips.length) {
  fail(
    `registry totalled ${expectedTotal} clips but web/registry.json describes ${clips.length}`,
  );
}
if (clips.length !== EXPECTED_CLIPS) {
  fail(`expected ${EXPECTED_CLIPS} clips, found ${clips.length}`);
}

// --- Fixtures (generated; may be absent) ------------------------------------
const fixturesPath = path.join(ROOT, "fixtures", "manifest.json");
let fixtureCounts = new Map();
if (fs.existsSync(fixturesPath)) {
  const manifest = JSON.parse(fs.readFileSync(fixturesPath, "utf8"));
  fixtureCounts = new Map(
    manifest.clips.map((clip) => [
      `${clip.voice}/${clip.phrase}`,
      clip.sampleCount,
    ]),
  );
  console.log(
    `    fixtures: ${fixtureCounts.size} clips from fixtures/manifest.json ` +
      `(${manifest.decoder?.signature ?? "unknown decoder"})`,
  );
} else {
  console.log(
    "    fixtures: SKIP (fixtures/manifest.json missing). " +
      "Generate it with: python3 scripts/make_fixtures.py",
  );
}

// --- Typed-error check ------------------------------------------------------
try {
  wasm.decode(new Uint8Array([0x00, 0x01, 0x02, 0x03]));
  fail("decode(junk) returned instead of throwing");
} catch (error) {
  if (error?.name !== "RaspDecodeError") {
    fail(`decode(junk) error name ${error?.name} != RaspDecodeError`);
  } else if (error.code !== "ContainerNotWebm") {
    fail(`decode(junk) error code ${error.code} != ContainerNotWebm`);
  } else if (typeof error.message !== "string" || error.message === "") {
    fail("decode(junk) typed error has no message");
  } else {
    console.log(`    errors: typed RaspDecodeError(${error.code}) — no trap`);
  }
}

// --- Full corpus ------------------------------------------------------------
const timings = [];
let decodedSeconds = 0;
let totalFrames = 0;
let failuresBefore = failures.length;
let decodedOk = 0;
let fixtureChecked = 0;
let fixtureFailures = 0;

const started = process.hrtime.bigint();

for (let i = 0; i < clips.length; i += 1) {
  const clip = clips[i];
  const file = path.join(ROOT, "web", clip.path);
  let bytes;
  try {
    bytes = fs.readFileSync(file);
  } catch (error) {
    fail(`${clip.id}: cannot read ${clip.path}: ${error.message}`);
    continue;
  }

  let decoded;
  let samples;
  const t0 = process.hrtime.bigint();
  try {
    decoded = wasm.decode(bytes);
    samples = decoded.samples();
  } catch (error) {
    fail(
      `${clip.id}: decode threw ${error?.name ?? "Error"}` +
        `${error?.code ? `(${error.code})` : ""}: ${error?.message ?? error}`,
    );
    continue;
  }
  const elapsedMs = Number(process.hrtime.bigint() - t0) / 1e6;
  timings.push(elapsedMs);

  const rate = decoded.sample_rate();
  const channels = decoded.channels();
  const frames = decoded.length();

  if (rate !== SAMPLE_RATE) fail(`${clip.id}: sample_rate ${rate} != 48000`);
  if (channels !== 1) fail(`${clip.id}: channels ${channels} != 1`);
  if (!Number.isInteger(frames) || frames <= 0) {
    fail(`${clip.id}: length ${frames} is not a positive frame count`);
  }
  if (samples.length !== frames * channels) {
    fail(
      `${clip.id}: samples().length ${samples.length} != length() * channels ` +
        `(${frames} * ${channels})`,
    );
  }

  const durationMs = (frames / SAMPLE_RATE) * 1000;
  if (durationMs < 200 || durationMs > 3000) {
    fail(`${clip.id}: implausible duration ${durationMs.toFixed(1)} ms`);
  }
  if (Number.isFinite(clip.durationMs)) {
    const deltaMs = Math.abs(durationMs - clip.durationMs);
    if (deltaMs > DURATION_TOLERANCE_MS) {
      fail(
        `${clip.id}: decoded ${durationMs.toFixed(1)} ms vs registry ` +
          `${clip.durationMs} ms (delta ${deltaMs.toFixed(1)} ms)`,
      );
    }
  }

  let peak = 0;
  for (let j = 0; j < samples.length; j += 1) {
    const value = samples[j];
    if (!Number.isFinite(value)) {
      fail(`${clip.id}: non-finite sample at index ${j}`);
      break;
    }
    const magnitude = Math.abs(value);
    if (magnitude > peak) peak = magnitude;
  }
  if (peak > 1.01) fail(`${clip.id}: peak ${peak.toFixed(4)} > 1.01`);

  const fixtureSamples = fixtureCounts.get(clip.id);
  if (fixtureSamples !== undefined) {
    fixtureChecked += 1;
    if (frames !== fixtureSamples) {
      fixtureFailures += 1;
      fail(
        `${clip.id}: ${frames} frames != fixture manifest ${fixtureSamples} ` +
          `(libopus reference)`,
      );
    }
  }

  decodedSeconds += frames / SAMPLE_RATE;
  totalFrames += frames;
  decoded.free();
  decodedOk += 1;

  const done = i + 1;
  if (done % 250 === 0 || done === clips.length) {
    process.stdout.write(`    decoded ${done}/${clips.length}\r`);
  }
}
process.stdout.write("\n");

const wallMs = Number(process.hrtime.bigint() - started) / 1e6;
const corpusFailures = failures.length - failuresBefore;

// Fixtures that were expected but not present in the registry are a mismatch
// too: the manifest is generated from the vendored corpus, so it cannot
// legitimately name a clip that is missing here.
for (const id of fixtureCounts.keys()) {
  if (!clips.some((clip) => clip.id === id)) {
    fixtureFailures += 1;
    fail(`fixtures/manifest.json names ${id}, which is not in the registry`);
  }
}

// --- Report -----------------------------------------------------------------
timings.sort((a, b) => a - b);
const total = timings.reduce((sum, value) => sum + value, 0);
const decodeMs = timings.length ? total : NaN;
const perSecondOfAudio = decodedSeconds > 0 ? total / decodedSeconds : NaN;
const realtimeFactor = decodedSeconds > 0 ? decodedSeconds / (total / 1000) : NaN;

console.log("");
console.log("==> results");
console.log(`    clips:        ${decodedOk}/${clips.length} decoded` +
  `${corpusFailures ? `, ${corpusFailures} failed` : ""}`);
console.log(`    audio:        ${decodedSeconds.toFixed(3)} s (${totalFrames} frames)`);
if (fixtureCounts.size > 0) {
  console.log(
    `    fixtures:     ${fixtureChecked - fixtureFailures}/${fixtureChecked} ` +
      `sample counts match libopus (manifest)`,
  );
}
if (timings.length > 0) {
  console.log(
    `    decode ms:    min ${timings[0].toFixed(3)}  ` +
      `p50 ${percentile(timings, 50).toFixed(3)}  ` +
      `mean ${(decodeMs / timings.length).toFixed(3)}  ` +
      `p95 ${percentile(timings, 95).toFixed(3)}  ` +
      `max ${timings[timings.length - 1].toFixed(3)}`,
  );
  console.log(
    `    throughput:   ${perSecondOfAudio.toFixed(3)} ms per second of audio ` +
      `(${realtimeFactor.toFixed(0)}× realtime, decode + Float32Array copy)`,
  );
  console.log(`    wall:         ${(wallMs / 1000).toFixed(2)} s including file reads`);
}
console.log(`    wasm:         ${rawBytes} B raw / ${gzipBytes} B gzip`);

if (failures.length > 0) {
  console.log("");
  console.log(`FAILED: ${failures.length} failure(s)`);
  for (const message of failures) console.log(`  - ${message}`);
  process.exit(1);
}

console.log("");
console.log(`PASS: ${clips.length}/${clips.length} clips decoded, zero failures.`);
