// Measure decoding in Node through the wasm-bindgen nodejs target.
// usage: node run_wasm.js <pkg-dir> <clip.weba> [iters] [out.f32]
const fs = require("fs");
const path = require("path");

const pkgDir = process.argv[2];
const clip = process.argv[3];
const iters = Number(process.argv[4] || 20);
const outPath = process.argv[5];

const mod = require(path.resolve(pkgDir, "rasp_decode_spike.js"));
const bytes = fs.readFileSync(clip);

// Warm-up (module instantiation is not part of the timing).
const first = mod.decode_weba(bytes);
const samples = first.length;

let best = Infinity;
let total = 0;
for (let i = 0; i < iters; i++) {
  const t = process.hrtime.bigint();
  const out = mod.decode_weba(bytes);
  const ms = Number(process.hrtime.bigint() - t) / 1e6;
  if (out.length !== samples) throw new Error("length changed between runs");
  best = Math.min(best, ms);
  total += ms;
}

const secs = samples / 48000;
console.log(
  JSON.stringify({
    backend: mod.backend(),
    samples,
    secs: Number(secs.toFixed(6)),
    min_ms: Number(best.toFixed(3)),
    mean_ms: Number((total / iters).toFixed(3)),
    ms_per_s: Number((best / secs).toFixed(3)),
  })
);

if (outPath) {
  const out = mod.decode_weba(bytes);
  fs.writeFileSync(outPath, Buffer.from(out.buffer, out.byteOffset, out.byteLength));
}
