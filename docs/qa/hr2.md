# HR-2 blind listening round — how to run it

**Task:** U-12a (the pack and harness; U-12 is the general QA page) · **Gate:** M2 (after P-09)
**Round file this feeds:** `docs/qa/round-2.md` · **Time:** about 15 minutes for 24 trials

This round judges the four shipped natural presets on the question the metrics cannot
answer: does the processed line still sound like the same person, or does it sound like
the pitch was moved (thin, chipmunk, giant)?

## Run it

```sh
python3 scripts/make_hr2_pack.py
open ~/rasp-hr2/index.html
# if the browser refuses local audio:  python3 -m http.server 8091 --directory ~/rasp-hr2
```

The script builds `rasp` in release mode if needed, renders the pack, verifies it, and
prints the trial table, the file count and the total size. Nothing is downloaded and
nothing is uploaded; the whole pack is about 3 MB on local disk.

## The round

Six voices, one phrase each, four presets: **24 trials**, plus one original per voice
(6 files, shared by the four trials of that voice).

| voice | phrase | why it is in the set |
|---|---|---|
| `af_heart` | `watch-this` | plosives + fricatives, high F0 |
| `am_onyx` | `got-time` | low anchor; `shift-up` is the known p90-failing cell (28.58 c) |
| `bf_alice` | `good-luck` | British F, stress case |
| `af_aoede` | `good-to-see` | high F0, formant stress |
| `zf_xiaoni` | `greet` | Mandarin, shortest clip |
| `jf_nezumi` | `be-friends` | Japanese, cleanest voice |

Order is shuffled from `--seed` (default `20261028`) and the preset is never shown while
rating. One trial at a time, five questions, all five required before **Next/Finish**.
After the last trial the page shows per-preset means and offers the same CSV download.

## What the tester must know that the page does not say

- **A is always the untouched source clip; B is the processed one.** The page labels
  them only A and B so the comparison stays blind. Q3 ("consonants") and Q1 ("same
  person") are about B relative to A.
- **The preset is hidden, not secret.** It is base64 in the page's inline data (and
  absent from every file name), so a determined look at `view-source` can decode it.
  Do not do that during the round.
- **The seed identifies the round.** `localStorage` resumes only the same seed + same
  trial order. Re-running the script with the same seed reproduces the order; a new
  seed starts a new round.
- **`am_onyx` + `shift-up` is the known weak cell** (p90 28.58 c, median −0.12 c, no
  octave errors). Listen for wobble/roughness on the low male voice specifically; that
  is a measured product caveat, not a surprise.
- **`zf_xiaoni` and `jf_nezumi` speak the Mandarin/Japanese line**, not English. Judge
  the voice, not the words.
- **Only completed trials are exported.** The export button works mid-round; a trial
  counts once all five answers are set.
- Keyboard: `A` play A · `B` play B · `R` replay A then B · `1`–`5` answer the first
  unanswered question · `→` next (only when all five are answered).
- If the download does not appear, the end-of-round screen has a selectable copy of the
  CSV under "If the download does not appear".

## CSV

Header and column order are fixed:

```
trial,voice,phrase,preset,order,q1,q2,q3,q4,q5,notes,utc
```

| column | meaning |
|---|---|
| `trial` | stable id from the fixed grid (`t01`…`t24`) |
| `voice`, `phrase` | corpus ids |
| `preset` | `shift-up` / `shift-down` / `bright` / `deep` (revealed only here and on the summary) |
| `order` | 1-based presentation position in this seed's shuffle |
| `q1` | 1 clearly same person … 5 clearly different person |
| `q2` | 1 just pitch (thin/chipmunk) … 5 convincingly bigger/smaller body |
| `q3` | 1 crisper · 2 unchanged · 3 clickier |
| `q4` | 1 no artifact · 2 mild · 3 obvious |
| `q5` | 1 yes, ship · 2 maybe · 3 no |
| `notes` | the optional "where" text from Q4 (CSV-quoted) |
| `utc` | ISO-8601 UTC time the trial was completed |

Rows are in presentation order. Every voice/preset cell has 6 rows once the round is
complete, so per-preset means are directly comparable.

## After the round

Hand the CSV to the PM. Findings become `QA-HR2-<n>` with a severity (`blocker`,
`major`, `minor`, `wontfix`) and land in `docs/qa/round-2.md`; U-12/Q-04 consume the
same CSV shape later. Nothing in this round is a release sign-off by itself.
