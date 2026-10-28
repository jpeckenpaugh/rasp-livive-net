//! Tests for the seeded voice-variant generator (PLAN V-01, D23/D24).
//!
//! # The drift guards
//!
//! * [`golden_vectors`] pins 20 `(seed, voice) → (pitch, formant, tilt)` step
//!   triples and their FNV digest. The table is integer step counts, not `f32`
//!   text, so it compares exactly and reads as the quantisation grid.
//! * [`derive_is_deterministic`] pins the digest of 1,000 specs. Because the
//!   digest is a constant, two `cargo test` runs in **separate processes**
//!   compare it as well; in-process it is derived twice.
//! * [`sweep_is_valid`] checks 1,000 seeds × 3 voices: finite, on the grid,
//!   inside D1 at the drawn pitch, and in range.
//! * [`archetype_distribution`] reports the 3,000-draw share of each archetype
//!   and enforces 3 %–15 % (uniform is 8.33 %).
//! * [`archetypes_are_distinct`] enforces the 1.5 st / 4 % rule over all pairs.
//! * [`combination_counts`] enumerates the reachable quantised triples per
//!   archetype after D1 clamping and pins the totals.
//!
//! # Cross-target evidence
//!
//! `cargo build -p rasp-dsp --target wasm32-unknown-unknown` compiles the
//! crate. The two digests were also **executed inside wasm** under Node 26 with
//! a throwaway `cdylib` (outside the repo) that calls `derive` and reproduces
//! [`spec_digest`]: golden `0xbb56cc4fc712b8cd`, sweep
//! `0x119b74208b006bce`, and the first spec's `pitch_st`/`formant_ratio` bit
//! patterns `0xc039999a` / `0x3f6f5c29` all matched the native values. The wasm
//! target therefore executes the same derivation, not merely the same source.
//!
//! # Regenerate the tables
//!
//! ```text
//! cargo test -p rasp-dsp print_regeneration_tables -- --ignored --nocapture
//! ```
//!
//! The printed step triples are the golden table; the printed digests are the
//! two constants below. Nothing else needs editing.

use super::*;
use crate::params::{MAX_PITCH_ST, MIN_PITCH_ST};
use crate::transform::{
    MAX_FORMANT_RATIO, MAX_TILT_DB_PER_OCT, MIN_FORMANT_RATIO, MIN_TILT_DB_PER_OCT,
};
use std::collections::BTreeMap;

/// The voices the 1,000-seed sweep runs over: the low, high and mid anchors
/// used by the project's other gates (`docs/corpus.md`).
const SWEEP_VOICES: [&str; 3] = ["am_onyx", "af_aoede", "af_heart"];

/// Seeds for the committed golden table: 0…15 plus four large/edge values.
const GOLDEN_SEEDS: [u64; 20] = [
    0,
    1,
    2,
    3,
    4,
    5,
    6,
    7,
    8,
    9,
    10,
    11,
    12,
    13,
    14,
    15,
    0xDEAD_BEEF_CAFE_F00D,
    u64::MAX,
    0x8000_0000_0000_0000,
    12_345_678_901_234_567_890,
];

/// Voices for the golden table, cycling over the corpus's extremes and the
/// curated calibration set.
const GOLDEN_VOICES: [&str; 8] = [
    "am_onyx",
    "af_aoede",
    "af_heart",
    "am_eric",
    "bf_alice",
    "jf_nezumi",
    "zm_yunxia",
    "hf_alpha",
];

/// `(seed, voice, pitch_steps, formant_steps, tilt_steps)`.
const GOLDEN_VECTORS: [(u64, &str, i32, i32, i32); 20] = [
    (0, "am_onyx", -58, 187, -2),
    (1, "af_aoede", 29, 219, -1),
    (2, "af_heart", -50, 188, -6),
    (3, "am_eric", 27, 234, 9),
    (4, "bf_alice", 25, 240, 12),
    (5, "jf_nezumi", -4, 203, 3),
    (6, "zm_yunxia", 35, 228, 2),
    (7, "hf_alpha", -33, 176, 4),
    (8, "am_onyx", 59, 242, 6),
    (9, "af_aoede", 111, 219, 6),
    (10, "af_heart", 105, 227, 6),
    (11, "am_eric", -34, 191, -12),
    (12, "bf_alice", -52, 176, -8),
    (13, "jf_nezumi", -61, 177, 0),
    (14, "zm_yunxia", 44, 219, -1),
    (15, "hf_alpha", 34, 227, 4),
    (0xDEAD_BEEF_CAFE_F00D, "am_onyx", -66, 176, -1),
    (u64::MAX, "af_aoede", -46, 171, 0),
    (0x8000_0000_0000_0000, "af_heart", 89, 223, 0),
    (12_345_678_901_234_567_890, "am_eric", 42, 228, 4),
];

/// FNV-1a 64 over the 20 golden specs, canonical byte encoding.
const GOLDEN_DIGEST: u64 = 0xbb56_cc4f_c712_b8cd;

/// FNV-1a 64 over `seeds 0..1000` on `am_onyx`.
const SWEEP_DIGEST: u64 = 0x119b_7420_8b00_6bce;

/// Reachable quantised triples per archetype (discriminant order), after D1
/// clamping. See [`combination_counts`].
const COMBINATION_COUNTS: [u32; 12] = [
    1701, // MascDeep: D1's floor clamps the formant jitter at low pitches
    3069, // MascMid
    3069, // MascBright
    3069, // MascYoung
    3069, // FemLow
    3069, // FemMid
    3069, // FemBright
    2610, // FemAiry: D1's ceiling clamps the formant jitter at high pitches
    3069, // NeutralMid
    3069, // NeutralBright
    3042, // Child: D1's narrowing top row clips a few formant values
    3069, // Elder
];

/// Total reachable triples over all twelve archetypes.
const TOTAL_COMBINATIONS: u32 = 34_974;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Canonical digest of a sequence of specs: schema, voice, archetype index and
/// the three integer step counts. Seeds are not stored in a spec, so the caller
/// controls the sequence.
fn spec_digest<'a>(specs: impl IntoIterator<Item = VariantSpec<'a>>) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for spec in specs {
        hash = fnv1a64(hash, &spec.schema.to_le_bytes());
        hash = fnv1a64(hash, spec.base_voice.as_bytes());
        hash = fnv1a64(hash, &[spec.archetype.index() as u8]);
        hash = fnv1a64(hash, &spec.pitch_steps().to_le_bytes());
        hash = fnv1a64(hash, &spec.formant_steps().to_le_bytes());
        hash = fnv1a64(hash, &spec.tilt_steps().to_le_bytes());
    }
    hash
}

/// D1's validated formant window at `pitch_st`, computed independently of the
/// implementation with `f64` arithmetic.
fn d1_region(pitch_st: f32) -> (f64, f64) {
    let t = (f64::from(pitch_st.abs()) / 7.0).min(1.0);
    (0.75 + 0.25 * t, 1.33 - 0.23 * t)
}

fn on_grid(value: f32, step: f32) -> bool {
    let steps = (value / step).round();
    (value - steps * step).abs() <= 1e-5
}

/// All `(pitch offset, formant offset, tilt offset)` triples an archetype can
/// reach, after the same integer clamping `derive` applies.
fn reachable_offsets(archetype: Archetype) -> usize {
    let nominal = archetype.nominal();
    let mut combinations = 0usize;
    for pitch_offset in -PITCH_JITTER_STEPS..=PITCH_JITTER_STEPS {
        let pitch_steps =
            (nominal.pitch_steps + pitch_offset).clamp(MIN_PITCH_STEPS, MAX_PITCH_STEPS);
        let (low, high) = d1_formant_bounds(pitch_steps);
        let low = low.clamp(MIN_FORMANT_STEPS, MAX_FORMANT_STEPS);
        let high = high.clamp(MIN_FORMANT_STEPS, MAX_FORMANT_STEPS);
        let formant_low = (nominal.formant_steps - FORMANT_JITTER_STEPS).max(low);
        let formant_high = (nominal.formant_steps + FORMANT_JITTER_STEPS).min(high);
        let formants = if formant_low <= formant_high {
            (formant_high - formant_low + 1) as usize
        } else {
            0
        };
        combinations += formants * (2 * TILT_JITTER_STEPS + 1) as usize;
    }
    combinations
}

// ---------------------------------------------------------------------------
// Contract tests
// ---------------------------------------------------------------------------

#[test]
fn current_schema_matches_derive() {
    let a = derive(7, "am_onyx");
    let b = derive_with_schema(VARIANT_SCHEMA, 7, "am_onyx").expect("current schema");
    assert_eq!(a, b);
    assert_eq!(a.schema, VARIANT_SCHEMA);
    assert!(a.is_current_schema());
    assert_eq!(a.base_voice, "am_onyx");
}

#[test]
fn unsupported_schemas_are_typed_errors() {
    for schema in [0u16, 2, 999, u16::MAX] {
        let error = derive_with_schema(schema, 0, "am_onyx").expect_err("must reject");
        assert_eq!(
            error,
            VariantError::UnsupportedSchema {
                schema,
                supported: VARIANT_SCHEMA
            }
        );
        assert!(error.to_string().contains("unsupported variant schema"));
    }
}

#[test]
fn arbitrary_inputs_never_panic() {
    let voices = ["", "am_onyx", "not_a_voice", "🎤", "a-very-long-voice-id-…"];
    let seeds = [0u64, 1, u64::MAX, 0x8000_0000_0000_0000];
    for schema in 0..=3u16 {
        for seed in seeds {
            for voice in voices {
                let _ = derive_with_schema(schema, seed, voice);
            }
        }
    }
    for seed in seeds {
        for voice in voices {
            let spec = derive(seed, voice);
            assert_eq!(spec.base_voice, voice);
            assert!(spec.pitch_st.is_finite());
            assert!(spec.formant_ratio.is_finite());
            assert!(spec.tilt_db_per_oct.is_finite());
        }
    }
}

#[test]
fn seed_and_voice_both_change_the_variant() {
    assert_ne!(derive(0, "am_onyx"), derive(0, "af_heart"));
    assert_ne!(derive(0, "am_onyx"), derive(1, "am_onyx"));
    assert_ne!(derive(0, "am_onyx"), derive(0, "am_ony"));
}

#[test]
fn headroom_is_not_applied_here() {
    // The spec carries exactly the drawn point; D22's `w` belongs to the
    // renderer, so a non-zero pitch must not be scaled here.
    let spec = derive(0xDEAD_BEEF, "am_onyx");
    let params = spec.to_params();
    assert_eq!(params.pitch_st, spec.pitch_st);
    assert_eq!(params.formant_ratio, spec.formant_ratio);
    assert_eq!(params.tilt_db_per_oct, spec.tilt_db_per_oct);
    assert!(
        spec.pitch_st.abs() <= MAX_PITCH_ST,
        "{} > {}",
        spec.pitch_st,
        MAX_PITCH_ST
    );
}

#[test]
fn step_constants_span_the_documented_ranges() {
    assert_eq!(
        steps_to_f32(MIN_PITCH_STEPS, PITCH_STEP),
        MIN_PITCH_ST,
        "pitch floor step count"
    );
    assert_eq!(
        steps_to_f32(MAX_PITCH_STEPS, PITCH_STEP),
        MAX_PITCH_ST,
        "pitch ceiling step count"
    );
    assert_eq!(
        steps_to_f32(MIN_FORMANT_STEPS, FORMANT_STEP),
        MIN_FORMANT_RATIO
    );
    assert_eq!(
        steps_to_f32(MAX_FORMANT_STEPS, FORMANT_STEP),
        MAX_FORMANT_RATIO
    );
    assert_eq!(steps_to_f32(MIN_TILT_STEPS, TILT_STEP), MIN_TILT_DB_PER_OCT);
    assert_eq!(steps_to_f32(MAX_TILT_STEPS, TILT_STEP), MAX_TILT_DB_PER_OCT);
}

#[test]
fn archetype_ids_round_trip() {
    for archetype in Archetype::ALL {
        assert_eq!(Archetype::from_id(archetype.id()), Some(archetype));
        assert_eq!(archetype.to_string(), archetype.id());
        let nominal = archetype.nominal();
        assert!(nominal.pitch_steps.abs() <= MAX_PITCH_STEPS);
    }
    assert_eq!(Archetype::from_id("unknown"), None);
    assert_eq!(Archetype::ALL.len(), 12);
    for (index, archetype) in Archetype::ALL.into_iter().enumerate() {
        assert_eq!(archetype.index(), index);
    }
}

// ---------------------------------------------------------------------------
// Validity
// ---------------------------------------------------------------------------

#[test]
fn sweep_is_valid() {
    let mut checked = 0usize;
    for seed in 0..1_000u64 {
        for voice in SWEEP_VOICES {
            let spec = derive(seed, voice);
            assert_eq!(spec.schema, VARIANT_SCHEMA);
            assert_eq!(spec.base_voice, voice);
            assert!(spec.pitch_st.is_finite());
            assert!(spec.formant_ratio.is_finite());
            assert!(spec.tilt_db_per_oct.is_finite());

            assert!(on_grid(spec.pitch_st, PITCH_STEP), "{spec:?}");
            assert!(on_grid(spec.formant_ratio, FORMANT_STEP), "{spec:?}");
            assert!(on_grid(spec.tilt_db_per_oct, TILT_STEP), "{spec:?}");

            assert!(
                (MIN_PITCH_ST..=MAX_PITCH_ST).contains(&spec.pitch_st),
                "{spec:?}"
            );
            assert!(
                (MIN_FORMANT_RATIO..=MAX_FORMANT_RATIO).contains(&spec.formant_ratio),
                "{spec:?}"
            );
            assert!(
                (MIN_TILT_DB_PER_OCT..=MAX_TILT_DB_PER_OCT).contains(&spec.tilt_db_per_oct),
                "{spec:?}"
            );

            // D1's validated region, at the drawn pitch.
            let (low, high) = d1_region(spec.pitch_st);
            let ratio = f64::from(spec.formant_ratio);
            assert!(
                ratio >= low - 1e-6 && ratio <= high + 1e-6,
                "{} at {:+.2} st outside D1 {low:.4}..{high:.4}",
                spec.formant_ratio,
                spec.pitch_st
            );
            checked += 1;
        }
    }
    assert_eq!(checked, 3_000);
}

#[test]
fn jitter_stays_inside_the_documented_windows() {
    for seed in 0..2_000u64 {
        for voice in SWEEP_VOICES {
            let spec = derive(seed, voice);
            let nominal = spec.archetype.nominal();
            assert!(
                (spec.pitch_steps() - nominal.pitch_steps).abs() <= PITCH_JITTER_STEPS,
                "{spec:?} pitch outside jitter"
            );
            assert!(
                (spec.formant_steps() - nominal.formant_steps).abs() <= FORMANT_JITTER_STEPS,
                "{spec:?} formant outside jitter"
            );
            assert!(
                (spec.tilt_steps() - nominal.tilt_steps).abs() <= TILT_JITTER_STEPS,
                "{spec:?} tilt outside jitter"
            );
        }
    }
}

#[test]
fn archetypes_are_distinct() {
    // 1.5 st = 30 pitch steps; 4 % = 8 formant steps.
    let mut closest = (f64::INFINITY, Archetype::MascDeep, Archetype::MascMid);
    for (index, a) in Archetype::ALL.into_iter().enumerate() {
        for b in &Archetype::ALL[index + 1..] {
            let na = a.nominal();
            let nb = b.nominal();
            let dp = (na.pitch_steps - nb.pitch_steps).abs();
            let df = (na.formant_steps - nb.formant_steps).abs();
            assert!(
                dp >= 30 || df >= 8,
                "{a} vs {b}: Δpitch {dp} steps, Δformant {df} steps"
            );
            // Normalised slack: 1.0 means exactly on the constraint boundary.
            let slack = f64::from(dp).max(f64::from(df) * 30.0 / 8.0) / 30.0;
            if slack < closest.0 {
                closest = (slack, a, *b);
            }
        }
    }
    println!(
        "closest archetype pair: {} vs {} (normalised distance {:.2}× the 1.5 st / 4 % bound)",
        closest.1, closest.2, closest.0
    );
}

#[test]
fn combination_counts() {
    let mut total = 0usize;
    for (index, archetype) in Archetype::ALL.into_iter().enumerate() {
        let count = reachable_offsets(archetype);
        assert!(count > 0, "{archetype} has no reachable combinations");
        assert_eq!(
            count, COMBINATION_COUNTS[index] as usize,
            "{archetype} count drifted"
        );
        total += count;
        println!(
            "  {:16} {:5} triples (unclamped max {})",
            archetype.id(),
            count,
            31 * 11 * 9
        );
    }
    assert_eq!(total, TOTAL_COMBINATIONS as usize);
    println!("  total            {total} triples");
}

// ---------------------------------------------------------------------------
// Distribution
// ---------------------------------------------------------------------------

#[test]
fn archetype_distribution() {
    let mut counts: BTreeMap<Archetype, usize> = BTreeMap::new();
    let mut total = 0usize;
    for seed in 0..1_000u64 {
        for voice in SWEEP_VOICES {
            *counts.entry(derive(seed, voice).archetype).or_default() += 1;
            total += 1;
        }
    }
    assert_eq!(total, 3_000);
    println!("archetype distribution over {total} draws:");
    for archetype in Archetype::ALL {
        let count = counts.get(&archetype).copied().unwrap_or(0);
        let share = 100.0 * count as f64 / total as f64;
        println!("  {:16} {count:5}  {share:5.2} %", archetype.id());
        assert!(
            (3.0..=15.0).contains(&share),
            "{} share {share:.2} % outside 3–15 %",
            archetype.id()
        );
    }
}

// ---------------------------------------------------------------------------
// Drift guards
// ---------------------------------------------------------------------------

#[test]
fn golden_vectors() {
    for (index, &(seed, voice, pitch, formant, tilt)) in GOLDEN_VECTORS.iter().enumerate() {
        let spec = derive(seed, voice);
        assert_eq!(
            (spec.pitch_steps(), spec.formant_steps(), spec.tilt_steps()),
            (pitch, formant, tilt),
            "golden vector {index}: seed {seed}, voice {voice}"
        );
        assert_eq!(spec.schema, VARIANT_SCHEMA);
    }

    // Fold all twenty in table order.
    let all: Vec<VariantSpec<'_>> = GOLDEN_VECTORS
        .iter()
        .map(|&(seed, voice, _, _, _)| derive(seed, voice))
        .collect();
    let digest = spec_digest(all);
    println!("golden-vector digest (20 specs): {digest:#018x}");
    assert_eq!(digest, GOLDEN_DIGEST, "golden digest drifted");
}

#[test]
fn derive_is_deterministic() {
    let first: Vec<VariantSpec<'_>> = (0..1_000u64).map(|seed| derive(seed, "am_onyx")).collect();
    let second: Vec<VariantSpec<'_>> = (0..1_000u64).map(|seed| derive(seed, "am_onyx")).collect();
    assert_eq!(first, second);
    let digest = spec_digest(first.iter().copied());
    println!("1,000-spec digest: {digest:#018x}");
    assert_eq!(digest, SWEEP_DIGEST);
    println!("20-spec digest:    {:#018x}", GOLDEN_DIGEST);
}

// ---------------------------------------------------------------------------
// Table regeneration
// ---------------------------------------------------------------------------

#[test]
#[ignore = "table regeneration: cargo test -p rasp-dsp print_regeneration_tables -- --ignored --nocapture"]
fn print_regeneration_tables() {
    println!("GOLDEN_VECTORS (seed, voice, pitch_steps, formant_steps, tilt_steps):");
    for (index, &seed) in GOLDEN_SEEDS.iter().enumerate() {
        let voice = GOLDEN_VOICES[index % GOLDEN_VOICES.len()];
        let spec = derive(seed, voice);
        println!(
            "    ({seed}, {voice:?}, {}, {}, {}),",
            spec.pitch_steps(),
            spec.formant_steps(),
            spec.tilt_steps()
        );
    }
    let golden: Vec<VariantSpec<'_>> = GOLDEN_SEEDS
        .iter()
        .enumerate()
        .map(|(index, &seed)| derive(seed, GOLDEN_VOICES[index % GOLDEN_VOICES.len()]))
        .collect();
    println!(
        "\nconst GOLDEN_DIGEST: u64 = {:#018x};",
        spec_digest(golden)
    );

    let sweep: Vec<VariantSpec<'_>> = (0..1_000u64).map(|seed| derive(seed, "am_onyx")).collect();
    println!("const SWEEP_DIGEST: u64 = {:#018x};", spec_digest(sweep));

    println!("\nCOMBINATION_COUNTS:");
    let mut total = 0usize;
    for archetype in Archetype::ALL {
        let count = reachable_offsets(archetype);
        total += count;
        println!("    // {:16} {count:5}", archetype.id());
    }
    println!("const TOTAL_COMBINATIONS: u32 = {total};");
}
