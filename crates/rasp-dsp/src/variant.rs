//! Seeded voice-variant generation (PLAN V-01, decisions D23/D24).
//!
//! The product is DiceBear for voices: `(base_voice, npc_seed) → stable
//! parameter set → audio`. This module is the `(seed, voice) → params` half.
//! It is a **pure function**: no clocks, no threads, no floating-point random
//! draws, no platform hashing. `derive(seed, voice)` returns the same
//! [`VariantSpec`] bits on macOS, Linux and `wasm32` (D24), and
//! [`derive_with_schema`] is the versioned entry point that a stored NPC record
//! uses.
//!
//! ```text
//! (schema, seed, base_voice) ──► FNV-1a 64 ──► SplitMix64 streams
//!                                                     │
//!                        archetype + integer jitter ──┴──► quantised steps
//!                                                                 │
//!                                                   clamp to D1 region
//!                                                                 │
//!                                                       VariantSpec (f32)
//! ```
//!
//! # Determinism (D24)
//!
//! * The seed hash is **FNV-1a 64** over a fixed tag, the schema (`u16` LE),
//!   the seed (`u64` LE) and the voice id's UTF-8 bytes. `std::collections::
//!   hash::DefaultHasher` is deliberately **not** used: its output is not a
//!   stable contract across Rust versions or platforms.
//! * Four independent **SplitMix64** streams are derived from that hash, one per
//!   draw (archetype, pitch, formant, tilt), so adding a draw later cannot
//!   shift the existing ones.
//! * Every draw is an integer. Jitter is applied in **quantised step units**
//!   (pitch 0.05 st, formant 0.005, tilt 0.25 dB/oct), clamping happens in
//!   integer space, and only the final in-range step counts are converted to
//!   `f32` (`steps as f32 * STEP`). Integer→`f32` conversion and a single
//!   IEEE-754 multiplication are exactly reproducible; no transcendental
//!   function is on the path.
//! * [`VARIANT_SCHEMA`] is part of the hash **and** of the returned spec, so a
//!   schema bump is a deliberate, detectable re-voicing (D24: resolved
//!   parameters are stored alongside the NPC record).
//!
//! Golden vectors, a 1,000-seed digest and the cross-platform build are in
//! `variant/tests.rs`.
//!
//! # Archetypes
//!
//! Twelve natural archetypes, each a nominal point plus a jitter window of
//! **±0.75 st**, **±0.025 formant ratio** (≈ ±2.5 % near 1.0) and
//! **±1.0 dB/oct**. Nominal points are listed in increasing pitch order. The
//! jittered point is clamped to D1's validated region *at the drawn pitch*, so
//! the valid window shrinks where D1 narrows (deep archetypes at their lowest
//! pitches); a clamped draw is still inside the nominal ± jitter window, so it
//! never invents a value the archetype did not describe.
//!
//! | Archetype | pitch (st) | formant | tilt (dB/oct) | Intent |
//! |---|---|---|---|---|
//! | [`Archetype::MascDeep`] | −3.00 | 0.860 | −1.00 | large adult male: lowest F0, longest tract, dark source |
//! | [`Archetype::MascMid`] | −2.50 | 0.910 | −0.50 | ordinary adult male |
//! | [`Archetype::Elder`] | −2.00 | 0.960 | −2.00 | older speaker: slightly lower F0, dark source tilt |
//! | [`Archetype::MascBright`] | −1.60 | 1.040 | +1.00 | lighter male: shorter tract, brighter source |
//! | [`Archetype::MascYoung`] | −0.90 | 0.900 | +0.50 | young male: near corpus median, mild brightening |
//! | [`Archetype::NeutralMid`] | 0.00 | 1.000 | 0.00 | the unshifted centre of D1 |
//! | [`Archetype::FemLow`] | +0.50 | 1.060 | −0.50 | low adult female: slightly higher F0, shorter tract |
//! | [`Archetype::NeutralBright`] | +1.00 | 1.180 | +2.50 | androgynous, bright: short tract, strong HF |
//! | [`Archetype::FemMid`] | +1.50 | 1.120 | 0.00 | ordinary adult female |
//! | [`Archetype::FemAiry`] | +2.60 | 1.220 | +2.00 | small, airy: short tract plus a strong bright tilt |
//! | [`Archetype::FemBright`] | +3.00 | 1.070 | +1.50 | bright female: modest tract change, bright source |
//! | [`Archetype::Child`] | +5.00 | 1.120 | +1.00 | child: high F0, short tract (stop short of D1's narrow top row) |
//!
//! Every nominal point is inside D1's region at its own pitch, and every
//! jitter-plus-clamp result is inside it at the drawn pitch. The region rule is
//! the convex interpolation of D1's two measured rows:
//!
//! ```text
//! |pitch| ≤ 7 st:   formant_low(p)  = 0.75 + 0.25·|p|/7
//!                   formant_high(p) = 1.33 − 0.23·|p|/7
//! |pitch| > 7 st:   the ±7 row (1.00 … 1.10) is held
//! ```
//!
//! D1 warns that interpolation between the rows is **not** measurement
//! evidence; here it is used only as a containment rule for generated points,
//! which is the strongest region the project has actually validated.
//!
//! # Distinctness
//!
//! No two archetypes are within 1.5 st pitch **and** 4 % formant of each other
//! (tested in integer steps: `|Δpitch| ≥ 30 steps || |Δformant| ≥ 8 steps` over
//! all 66 pairs). A pair 1.0 st apart always differs by at least 0.04 in
//! formant ratio.
//!
//! # Combinations
//!
//! Unclamped, each archetype spans `31 × 11 × 9 = 3,069` distinct quantised
//! triples (pitch ±15 steps × formant ±5 steps × tilt ±4 steps). Clamping
//! against D1 removes some formant values at the outer pitches; the exact
//! per-archetype counts are asserted by `variant::tests::combination_counts`
//! and are reproduced here:
//!
//! | Archetype | Reachable triples | Why it is not 3,069 |
//! |---|---|---|
//! | [`Archetype::MascDeep`] | 1,701 | D1's floor rises past the formant window at the lowest pitches, so deep draws clamp upward |
//! | [`Archetype::MascMid`] | 3,069 | unclamped |
//! | [`Archetype::MascBright`] | 3,069 | unclamped |
//! | [`Archetype::MascYoung`] | 3,069 | unclamped |
//! | [`Archetype::FemLow`] | 3,069 | unclamped |
//! | [`Archetype::FemMid`] | 3,069 | unclamped |
//! | [`Archetype::FemBright`] | 3,069 | unclamped |
//! | [`Archetype::FemAiry`] | 2,610 | D1's ceiling falls below the top of the formant window at the highest pitches |
//! | [`Archetype::NeutralMid`] | 3,069 | unclamped |
//! | [`Archetype::NeutralBright`] | 3,069 | unclamped |
//! | [`Archetype::Child`] | 3,042 | the narrowing top row clips a few formant values |
//! | [`Archetype::Elder`] | 3,069 | unclamped |
//! | **all twelve** | **34,974** | |
//!
//! # Headroom is not applied here (D22)
//!
//! This module produces the **requested** point. [`VariantSpec::to_params`]
//! returns it unchanged; the renderer applies D22's per-clip weight `w` at
//! render time ([`crate::params::NaturalRenderer`]). Duplicating that mapping
//! here would double-scale and would make the stored NPC record disagree with
//! what was rendered.
//!
//! # Base-voice resolution is not here (V-02)
//!
//! Any string is accepted as a voice id — empty, unknown, non-corpus. V-02 owns
//! `(seed) → base_voice` over the 54 corpus voices and its distribution
//! weighting; this module only answers "given this voice and seed, which
//! parameters?".

use std::fmt;

use crate::params::NaturalParams;

/// Version of the derivation contract (D24).
///
/// The schema is mixed into the seed hash and carried in every [`VariantSpec`].
/// Only [`VARIANT_SCHEMA`] is accepted; anything else is a typed
/// [`VariantError::UnsupportedSchema`], never a silent reinterpretation.
pub const VARIANT_SCHEMA: u16 = 1;

/// Pitch quantisation step, in semitones (D24).
pub const PITCH_STEP: f32 = 0.05;
/// Formant-ratio quantisation step (D24).
pub const FORMANT_STEP: f32 = 0.005;
/// Spectral-tilt quantisation step, in dB per octave (D24).
pub const TILT_STEP: f32 = 0.25;

/// Pitch jitter half-window, in steps: ±0.75 st.
pub const PITCH_JITTER_STEPS: i32 = 15;
/// Formant jitter half-window, in steps: ±0.025 ratio.
pub const FORMANT_JITTER_STEPS: i32 = 5;
/// Tilt jitter half-window, in steps: ±1.0 dB/oct.
pub const TILT_JITTER_STEPS: i32 = 4;

/// FNV-1a 64 offset basis.
const FNV_OFFSET_BASIS: u64 = 0xCBF2_9CE4_8422_2325;
/// FNV-1a 64 prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;
/// SplitMix64 increment (the golden-ratio constant).
const SPLITMIX_GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;
/// Domain tag for the whole variant hash, so it cannot collide with another use
/// of FNV in the crate.
const HASH_TAG: &[u8] = b"rasp.variant";
/// Domain tags for the four independent draw streams.
const DOMAIN_ARCHETYPE: u64 = 0x0A7E_0000_0000_0001;
const DOMAIN_PITCH: u64 = 0x0A7E_0000_0000_0002;
const DOMAIN_FORMANT: u64 = 0x0A7E_0000_0000_0003;
const DOMAIN_TILT: u64 = 0x0A7E_0000_0000_0004;

/// Pitch steps that span D1's measured row, 7 st = 140 steps.
const D1_PITCH_STEPS: i64 = 140;
/// Formant steps at 0.75 (the pitch-0 lower bound).
const D1_FORMANT_LOW_STEPS: i64 = 150;
/// Formant steps at 1.33 (the pitch-0 upper bound).
const D1_FORMANT_HIGH_STEPS: i64 = 266;
/// Steps the lower bound rises between pitch 0 and ±7 st: 0.25 / 0.005.
const D1_LOW_RISE_STEPS: i64 = 50;
/// Steps the upper bound falls between pitch 0 and ±7 st: 0.23 / 0.005.
const D1_HIGH_FALL_STEPS: i64 = 46;

/// Lowest pitch step accepted, 0.05 st units (−12 st).
///
/// Written as an integer because `(−12.0f32 / 0.05f32) as i32` truncates to
/// −239: `0.05f32` is slightly above one twentieth, so the quotient is slightly
/// below 240 in magnitude. `step_constants_span_the_documented_ranges` pins
/// these against [`MIN_PITCH_ST`] and friends.
const MIN_PITCH_STEPS: i32 = -240;
/// Highest pitch step accepted, 0.05 st units (+12 st).
const MAX_PITCH_STEPS: i32 = 240;
/// Lowest formant step accepted (0.75).
const MIN_FORMANT_STEPS: i32 = 150;
/// Highest formant step accepted (1.33).
const MAX_FORMANT_STEPS: i32 = 266;
/// Lowest tilt step accepted (−6 dB/oct).
const MIN_TILT_STEPS: i32 = -24;
/// Highest tilt step accepted (+6 dB/oct).
const MAX_TILT_STEPS: i32 = 24;

/// The twelve natural archetypes (PLAN V-01).
///
/// The discriminant order is the module's declaration order; it is part of the
/// derivation contract because the archetype index is drawn from a stream and
/// recorded in [`VariantSpec`]. Do not reorder without bumping
/// [`VARIANT_SCHEMA`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Archetype {
    /// Large adult male.
    MascDeep,
    /// Ordinary adult male.
    MascMid,
    /// Lighter, brighter male.
    MascBright,
    /// Young male, near the corpus median.
    MascYoung,
    /// Low adult female.
    FemLow,
    /// Ordinary adult female.
    FemMid,
    /// Bright adult female.
    FemBright,
    /// Small, airy adult female.
    FemAiry,
    /// The neutral centre of D1.
    NeutralMid,
    /// Androgynous, bright.
    NeutralBright,
    /// Child.
    Child,
    /// Older speaker.
    Elder,
}

impl Archetype {
    /// Every archetype, in discriminant order.
    pub const ALL: [Archetype; 12] = [
        Archetype::MascDeep,
        Archetype::MascMid,
        Archetype::MascBright,
        Archetype::MascYoung,
        Archetype::FemLow,
        Archetype::FemMid,
        Archetype::FemBright,
        Archetype::FemAiry,
        Archetype::NeutralMid,
        Archetype::NeutralBright,
        Archetype::Child,
        Archetype::Elder,
    ];

    /// The archetype's index in [`ALL`](Archetype::ALL), `0..12`.
    pub const fn index(self) -> usize {
        self as usize
    }

    /// The stable wire id, kebab-case like the D12 preset ids.
    pub const fn id(self) -> &'static str {
        match self {
            Archetype::MascDeep => "masc-deep",
            Archetype::MascMid => "masc-mid",
            Archetype::MascBright => "masc-bright",
            Archetype::MascYoung => "masc-young",
            Archetype::FemLow => "fem-low",
            Archetype::FemMid => "fem-mid",
            Archetype::FemBright => "fem-bright",
            Archetype::FemAiry => "fem-airy",
            Archetype::NeutralMid => "neutral-mid",
            Archetype::NeutralBright => "neutral-bright",
            Archetype::Child => "child",
            Archetype::Elder => "elder",
        }
    }

    /// Parse a wire id; `None` for anything unknown (no panic, no guessing).
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|archetype| archetype.id() == id)
    }

    /// The nominal operating point, in quantised steps.
    pub const fn nominal(self) -> ArchetypeNominal {
        match self {
            Archetype::MascDeep => ArchetypeNominal::new(-60, 172, -4),
            Archetype::MascMid => ArchetypeNominal::new(-50, 182, -2),
            Archetype::MascBright => ArchetypeNominal::new(-32, 208, 4),
            Archetype::MascYoung => ArchetypeNominal::new(-18, 180, 2),
            Archetype::FemLow => ArchetypeNominal::new(10, 212, -2),
            Archetype::FemMid => ArchetypeNominal::new(30, 224, 0),
            Archetype::FemBright => ArchetypeNominal::new(60, 214, 6),
            Archetype::FemAiry => ArchetypeNominal::new(52, 244, 8),
            Archetype::NeutralMid => ArchetypeNominal::new(0, 200, 0),
            Archetype::NeutralBright => ArchetypeNominal::new(20, 236, 10),
            Archetype::Child => ArchetypeNominal::new(100, 224, 4),
            Archetype::Elder => ArchetypeNominal::new(-40, 192, -8),
        }
    }
}

impl fmt::Display for Archetype {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

/// An archetype's nominal point in quantised step units.
///
/// Steps are the derivation's integer currency: pitch in 0.05 st, formant in
/// 0.005, tilt in 0.25 dB/oct. Accessors convert to the `f32` parameter units
/// with one multiplication each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ArchetypeNominal {
    /// Pitch in 0.05 st steps.
    pub pitch_steps: i32,
    /// Formant ratio in 0.005 steps.
    pub formant_steps: i32,
    /// Tilt in 0.25 dB/oct steps.
    pub tilt_steps: i32,
}

impl ArchetypeNominal {
    const fn new(pitch_steps: i32, formant_steps: i32, tilt_steps: i32) -> Self {
        ArchetypeNominal {
            pitch_steps,
            formant_steps,
            tilt_steps,
        }
    }

    /// Nominal pitch shift, semitones.
    pub fn pitch_st(&self) -> f32 {
        steps_to_f32(self.pitch_steps, PITCH_STEP)
    }

    /// Nominal formant ratio.
    pub fn formant_ratio(&self) -> f32 {
        steps_to_f32(self.formant_steps, FORMANT_STEP)
    }

    /// Nominal tilt, dB per octave.
    pub fn tilt_db_per_oct(&self) -> f32 {
        steps_to_f32(self.tilt_steps, TILT_STEP)
    }
}

/// A derived, stable set of natural-mode parameters (D23/D24).
///
/// The fields are public so a UI or a stored record can read them directly. A
/// spec is only ever produced by [`derive`] / [`derive_with_schema`]; nothing
/// here validates on construction, but every derived value is already inside
/// D1's region and inside [`NaturalParams`]'s ranges.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VariantSpec<'a> {
    /// The derivation contract version (D24).
    pub schema: u16,
    /// The voice id the variant was derived from, exactly as passed in.
    pub base_voice: &'a str,
    /// The drawn archetype.
    pub archetype: Archetype,
    /// Pitch shift in semitones, on the 0.05 st grid.
    pub pitch_st: f32,
    /// Formant ratio, on the 0.005 grid.
    pub formant_ratio: f32,
    /// Spectral tilt in dB/oct, on the 0.25 grid. Positive is brighter.
    pub tilt_db_per_oct: f32,
}

impl VariantSpec<'_> {
    /// Pitch in 0.05 st steps.
    pub fn pitch_steps(&self) -> i32 {
        f32_to_steps(self.pitch_st, PITCH_STEP)
    }

    /// Formant ratio in 0.005 steps.
    pub fn formant_steps(&self) -> i32 {
        f32_to_steps(self.formant_ratio, FORMANT_STEP)
    }

    /// Tilt in 0.25 dB/oct steps.
    pub fn tilt_steps(&self) -> i32 {
        f32_to_steps(self.tilt_db_per_oct, TILT_STEP)
    }

    /// The requested natural-mode point, **without** D22 headroom scaling.
    ///
    /// The renderer applies `w` per clip; this is the stored/displayed point.
    pub fn to_params(&self) -> NaturalParams {
        NaturalParams::new(self.pitch_st, self.formant_ratio, self.tilt_db_per_oct)
    }

    /// `true` when this spec was produced by the current [`VARIANT_SCHEMA`].
    pub fn is_current_schema(&self) -> bool {
        self.schema == VARIANT_SCHEMA
    }
}

/// Errors from versioned derivation.
///
/// Every failure is typed; nothing in this module panics on public input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VariantError {
    /// The requested schema is not the one this build implements.
    UnsupportedSchema {
        /// The schema that was requested.
        schema: u16,
        /// The schema this build implements ([`VARIANT_SCHEMA`]).
        supported: u16,
    },
}

impl fmt::Display for VariantError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VariantError::UnsupportedSchema { schema, supported } => write!(
                f,
                "unsupported variant schema {schema} (this build implements {supported})"
            ),
        }
    }
}

impl std::error::Error for VariantError {}

/// Derive a variant with the current [`VARIANT_SCHEMA`].
///
/// Pure, total and deterministic: the same `(seed, base_voice)` returns the
/// same bits on every target (D24). Any voice string is accepted (V-02 owns
/// resolving a seed to a corpus voice).
pub fn derive<'a>(seed: u64, base_voice: &'a str) -> VariantSpec<'a> {
    derive_unchecked(VARIANT_SCHEMA, seed, base_voice)
}

/// Derive a variant for an explicit schema version.
///
/// Returns [`VariantError::UnsupportedSchema`] for any schema other than
/// [`VARIANT_SCHEMA`], including `0` and values above it. The check happens
/// before any draw, so a rejected request cannot return a half-validated spec.
pub fn derive_with_schema<'a>(
    schema: u16,
    seed: u64,
    base_voice: &'a str,
) -> Result<VariantSpec<'a>, VariantError> {
    if schema != VARIANT_SCHEMA {
        return Err(VariantError::UnsupportedSchema {
            schema,
            supported: VARIANT_SCHEMA,
        });
    }
    Ok(derive_unchecked(schema, seed, base_voice))
}

/// The derivation body, valid only for an already-checked schema.
fn derive_unchecked<'a>(schema: u16, seed: u64, base_voice: &'a str) -> VariantSpec<'a> {
    let base = seed_hash(schema, seed, base_voice);

    let mut archetype_stream = stream(base, DOMAIN_ARCHETYPE);
    let mut pitch_stream = stream(base, DOMAIN_PITCH);
    let mut formant_stream = stream(base, DOMAIN_FORMANT);
    let mut tilt_stream = stream(base, DOMAIN_TILT);

    let archetype =
        Archetype::ALL[(archetype_stream.next_u64() % Archetype::ALL.len() as u64) as usize];
    let nominal = archetype.nominal();

    let pitch_steps = (nominal.pitch_steps + draw_offset(&mut pitch_stream, PITCH_JITTER_STEPS))
        .clamp(MIN_PITCH_STEPS, MAX_PITCH_STEPS);

    let (low, high) = d1_formant_bounds(pitch_steps);
    let formant_steps = (nominal.formant_steps
        + draw_offset(&mut formant_stream, FORMANT_JITTER_STEPS))
    .clamp(low.min(high), high.max(low))
    .clamp(MIN_FORMANT_STEPS, MAX_FORMANT_STEPS);

    let tilt_steps = (nominal.tilt_steps + draw_offset(&mut tilt_stream, TILT_JITTER_STEPS))
        .clamp(MIN_TILT_STEPS, MAX_TILT_STEPS);

    VariantSpec {
        schema,
        base_voice,
        archetype,
        pitch_st: steps_to_f32(pitch_steps, PITCH_STEP),
        formant_ratio: steps_to_f32(formant_steps, FORMANT_STEP),
        tilt_db_per_oct: steps_to_f32(tilt_steps, TILT_STEP),
    }
}

/// FNV-1a 64 of `(HASH_TAG, schema, seed, voice)`.
fn seed_hash(schema: u16, seed: u64, base_voice: &str) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    hash = fnv1a64(hash, HASH_TAG);
    hash = fnv1a64(hash, &schema.to_le_bytes());
    hash = fnv1a64(hash, &seed.to_le_bytes());
    hash = fnv1a64(hash, base_voice.as_bytes());
    hash
}

/// Extend an FNV-1a 64 hash with `bytes`.
fn fnv1a64(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// A SplitMix64 stream, domain-separated from `base`.
fn stream(base: u64, domain: u64) -> SplitMix64 {
    SplitMix64::new(fnv1a64(base, &domain.to_le_bytes()))
}

/// Draw a uniform integer in `[-span, +span]`.
///
/// `% (2·span+1)` has a relative bias below `2^-60` for these spans, which is far
/// inside the "no observable skew" requirement of a 3,000-draw distribution
/// test and irrelevant for a per-NPC cast.
fn draw_offset(stream: &mut SplitMix64, span: i32) -> i32 {
    let width = (2 * span + 1) as u64;
    (stream.next_u64() % width) as i32 - span
}

/// D1's validated formant window at `pitch_steps`, in formant steps.
///
/// Linear interpolation between the measured rows (0 st: 0.75–1.33, ±7 st:
/// 1.00–1.10), held at the ±7 row further out. Bounds are rounded inward
/// (ceil the lower, floor the upper) so the returned integers are themselves
/// inside the region.
fn d1_formant_bounds(pitch_steps: i32) -> (i32, i32) {
    let magnitude = i64::from(pitch_steps.unsigned_abs()).min(D1_PITCH_STEPS);
    let low = D1_FORMANT_LOW_STEPS + ceil_div(D1_LOW_RISE_STEPS * magnitude, D1_PITCH_STEPS);
    let high = D1_FORMANT_HIGH_STEPS - ceil_div(D1_HIGH_FALL_STEPS * magnitude, D1_PITCH_STEPS);
    (low as i32, high as i32)
}

/// `ceil(numerator / denominator)` for non-negative operands.
fn ceil_div(numerator: i64, denominator: i64) -> i64 {
    (numerator + denominator - 1) / denominator
}

/// Convert an integer step count to its `f32` parameter value.
fn steps_to_f32(steps: i32, step: f32) -> f32 {
    steps as f32 * step
}

/// Recover the step count from a parameter value.
fn f32_to_steps(value: f32, step: f32) -> i32 {
    (value / step).round() as i32
}

/// SplitMix64 (Steele, Lea & Flood), the integer stream behind D24.
#[derive(Debug, Clone)]
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        SplitMix64 { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(SPLITMIX_GAMMA);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

#[cfg(test)]
mod tests;
