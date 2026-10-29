# Integration contract — `pp.livive.net` voice variants

Target: the Play Park prototype (`https://pp.livive.net/`). Each NPC already has
`recipe.seed` — a string, shown to the player as "same seed, same character,
forever". The voice uses **the same seed**, so look and voice are one identity.

```
npc.recipe.seed ──► RaspVoice.describe(seed) ──► params ──► render ──► AudioBuffer ──► play
```

## Hosting

Assets live at `https://rasp.livive.net/` (GitHub Pages, `access-control-allow-origin: *`):

| Path | Purpose |
|---|---|
| `/rasp-client.js` | ES module, the only file the game imports |
| `/wasm/rasp.js`, `/wasm/rasp_bg.wasm` | build output (D18 — not committed, built in CI) |
| `/registry.json` | 54 voices × 26 phrases, hooks, build id |
| `/audio/<voice>/<phrase>.weba` | 7.0 MB corpus, immutable per build id |

Cross-origin is fine on Pages today. If that ever changes, vendor `rasp-client.js`
plus `wasm/` into the pp repo; `baseUrl` is configurable for exactly that.

## Engine exports (live now)

The wasm is deployed at `https://rasp.livive.net/wasm/rasp.js` (CORS `*`). You can
call it directly, without the wrapper below:

```js
import init, { variant_schema, describe, render, render_params, effective_params }
  from 'https://rasp.livive.net/wasm/rasp.js';
await init();                       // loads ./wasm/rasp_bg.wasm next to rasp.js

describe(seed)                      // {seed, voiceId, archetype, schema, pitchSt, formantRatio, tiltDbPerOct}
render(webaBytes, seed)             // Float32Array, 48 kHz mono, seeded params
render_params(webaBytes, p, f, t)   // Float32Array, explicit pitch/formant/tilt
effective_params(webaBytes, p, f, t)// what headroom actually applied (D22)
variant_schema()                     // 1
```

`rasp-client.js` (the caching/playback wrapper) is not published yet; until it is,
use these exports plus the caching notes below.

## API

```js
import { createRaspVoice } from 'https://rasp.livive.net/rasp-client.js';
const voice = await createRaspVoice({ baseUrl: 'https://rasp.livive.net/' });
// voice.supported === false  → no wasm; speak() resolves false instead of throwing

const spec = voice.describe(npc.recipe.seed);
// { seed, voiceId, archetype, pitchSt, formantRatio, tiltDbPerOct, schema }

await voice.speak({ seed: npc.recipe.seed, hook: 'social.greet' });
// or: { seed, phrase: 'greet' }

const buffer = await voice.render({ seed, phrase: 'hello' }); // AudioBuffer @ 48 kHz
voice.prefetch([{ seed, hook: 'social.chat' }, { seed, phrase: 'yay' }]);
```

Rules the implementation must honour:

1. **Determinism.** `describe(seed)` is pure and identical on every platform and
   every session. `schema` is exposed; a schema bump may re-voice the cast and is
   a release note, not a silent change. Hosts that need permanence should store
   the returned `spec`.
2. **Autoplay.** No `AudioContext` is created until the first `speak()`/`render()`
   call, which must originate from a user gesture (pp: NPC click or keypress).
   `unlock()` exists for hosts that want to prime the context on an explicit
   "sound on" click.
3. **Never break the game.** Missing wasm, 404, decode failure, or a browser
   without WebAssembly must resolve `false` (and set `lastError`), not throw.
4. **Cache.** Rendered lines are cached in IndexedDB keyed by
   `(schema, seed, phrase, registryBuild)`. `prefetch` fills it during idle time.
   Cache is best-effort; the source of truth is the render.
5. **Hooks are semantic.** `hook` is one of the registry's 17 names
   (`social.greet`, `social.chat`, `minigame.win`, …). The candidate phrase is
   chosen deterministically from `(seed, hook, attempt)` so the same NPC says the
   same greeting for the same occasion, but not always the same line.
6. **No network at speak time** if the line is already cached and the corpus is
   vendored; otherwise one `.weba` fetch (~5 KB) plus a render.

## Cost

| Step | Measured |
|---|---|
| decode + render, 1 s line | ~100–200 ms in wasm (first play only) |
| cached play | < 5 ms |
| wasm payload | ~215 KB gzip |
| corpus | 7.0 MB, all 54 voices, all 26 lines |

## Minimal pp integration

```js
// once, at boot
const rasp = await createRaspVoice();

// in the existing NPC dialogue path
function npcSpeak(npc, hook = 'social.chat') {
  rasp.speak({ seed: npc.recipe.seed, hook });   // fire-and-forget
}

// "New look" already reshuffles the seed → call again and the voice follows
```

`seed` is a string; anything stable per NPC works. Do **not** use the NPC's
display name — names are reused across parks.
