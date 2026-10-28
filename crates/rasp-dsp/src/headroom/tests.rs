//! Tests for the D22 headroom mapping and the F0 measurement behind it.
//!
//! Nothing here is a perceptual claim: D22 is a *relative* strength rule, and
//! HR-2 judges whether it sounds like the same person. These tests pin the
//! arithmetic, the envelope consequence, the estimator's accuracy against the
//! official D17 ruler, and the renderer wiring.

use super::*;
use crate::params::{NaturalPreset, NaturalRenderReport, NaturalRenderer};
use crate::testutil;

const SR: u32 = 48_000;

/// The 54 voice medians of `docs/corpus.md` §2, sorted. These *are* the corpus
/// envelope: 79 Hz (`bm_lewis`) to 323 Hz (`zf_xiaoyi`), median 178 Hz.
const CORPUS_MEDIANS_HZ: [f32; 54] = [
    78.9, 86.4, 88.4, 113.7, 114.0, 117.5, 119.4, 120.3, 124.4, 128.0, 128.5, 129.2, 133.1, 134.6,
    136.1, 136.3, 136.7, 139.1, 140.9, 147.7, 152.2, 152.3, 161.6, 171.4, 171.7, 175.0, 176.1,
    179.3, 181.1, 181.2, 182.5, 190.7, 192.8, 202.8, 206.6, 210.1, 211.4, 214.2, 217.3, 221.3,
    225.9, 228.3, 239.3, 242.1, 248.3, 253.3, 266.6, 267.8, 270.3, 275.3, 282.6, 304.5, 315.5,
    323.1,
];

/// Per-clip median F0 of the 12 fixtures, measured by the **official D17 ruler**
/// (`scripts/metrics.py` → `f0_stats`, YIN with median smoothing) on
/// `fixtures/f32/*.f32`, 2026-10-28. These are the ground truth for the
/// estimator test below; the corpus *voice* medians are a different quantity
/// (a 26-clip pool vs one phrase) and are printed for context only.
const FIXTURE_MEDIANS_HZ: [(&str, f64); 12] = [
    ("af_aoede__you-got-me", 223.52),
    ("af_heart__friends-now", 228.80),
    ("am_eric__good-game", 163.08),
    ("am_onyx__got-time", 82.27),
    ("bf_alice__maybe-next-time", 235.72),
    ("bf_emma__nice-day", 186.80),
    ("ef_dora__see-you", 171.76),
    ("hf_alpha__good-to-see", 228.32),
    ("jf_nezumi__be-friends", 226.24),
    ("jm_kumo__watch-this", 97.15),
    ("zf_xiaoni__greet", 227.09),
    ("zm_yunxi__no-worries", 175.94),
];

/// Corpus voice median for the same fixture, for the report table only.
const CORPUS_VOICE_MEDIANS_HZ: [(&str, f64); 12] = [
    ("af_aoede__you-got-me", 221.3),
    ("af_heart__friends-now", 214.2),
    ("am_eric__good-game", 161.6),
    ("am_onyx__got-time", 88.4),
    ("bf_alice__maybe-next-time", 248.3),
    ("bf_emma__nice-day", 181.1),
    ("ef_dora__see-you", 179.3),
    ("hf_alpha__good-to-see", 239.3),
    ("jf_nezumi__be-friends", 228.3),
    ("jm_kumo__watch-this", 86.4),
    ("zf_xiaoni__greet", 242.1),
    ("zm_yunxi__no-worries", 175.0),
];

fn preset_nominal(preset: NaturalPreset) -> NaturalParams {
    preset.params()
}

fn effective(preset: NaturalPreset, f0_hz: f32) -> f32 {
    let nominal = preset_nominal(preset);
    nominal.pitch_st * headroom_weight(Some(f0_hz), nominal.pitch_st)
}

#[test]
fn weight_is_the_d22_formula() {
    let up_reference = room_up_hz(HEADROOM_REFERENCE_HZ);
    let down_reference = room_down_hz(HEADROOM_REFERENCE_HZ);
    println!("room_up(178) {up_reference:.3} st, room_down(178) {down_reference:.3} st");
    assert!((up_reference - 10.314).abs() < 0.01, "{up_reference}");
    assert!((down_reference - 14.058).abs() < 0.01, "{down_reference}");

    // The envelope edges have no room outward and full room inward.
    assert_eq!(room_up_hz(HEADROOM_HIGH_HZ), 0.0);
    assert_eq!(room_down_hz(HEADROOM_LOW_HZ), 0.0);
    assert!((room_up_hz(HEADROOM_LOW_HZ) - 24.377).abs() < 0.01);
    assert!((room_down_hz(HEADROOM_HIGH_HZ) - 24.377).abs() < 0.01);

    // A median voice keeps the whole nominal shift in either direction.
    for pitch in [-12.0f32, -4.0, -3.0, 0.0, 7.0, 12.0] {
        assert_eq!(headroom_weight(Some(178.0), pitch), 1.0, "pitch {pitch}");
    }

    // `deep` at the low anchor (88.4 Hz): w 0.138, −0.55 st (D22 quotes −0.55).
    let w = headroom_weight(Some(88.4), -4.0);
    assert!((w - 0.138).abs() < 0.002, "w {w}");
    assert!((-4.0 * w + 0.55).abs() < 0.02, "{}", -4.0 * w);

    // `shift-up` at the highest voice: no headroom at all.
    assert_eq!(headroom_weight(Some(323.1), 7.0), 0.0);
    assert_eq!(headroom_weight(Some(400.0), 7.0), 0.0);

    // `shift-up` on the 248 Hz stress voice: +3.1 st of the nominal +7.
    let w = headroom_weight(Some(248.3), 7.0);
    assert!((w - 0.441).abs() < 0.003, "w {w}");
    assert!((7.0 * w - 3.09).abs() < 0.02, "{}", 7.0 * w);

    // No direction (bright/natural) and no measurement both fall back to 1.
    assert_eq!(headroom_weight(Some(300.0), 0.0), 1.0);
    assert_eq!(headroom_weight(None, 7.0), 1.0);
    assert_eq!(headroom_weight(Some(f32::NAN), -4.0), 1.0);
    assert_eq!(headroom_weight(Some(0.0), -4.0), 1.0);
}

#[test]
fn scaling_is_linear_in_pitch_and_tilt_and_geometric_in_formant() {
    let nominal = NaturalParams::new(7.0, 1.10, 0.75);
    let scaled = scale_params(nominal, 0.5);
    assert!((scaled.pitch_st - 3.5).abs() < 1e-6);
    assert!((scaled.formant_ratio - 1.10f32.powf(0.5)).abs() < 1e-6);
    assert!((scaled.tilt_db_per_oct - 0.375).abs() < 1e-6);

    // No headroom is a bit-exact neutral point, not a tiny shift.
    let none = scale_params(nominal, 0.0);
    assert_eq!(none, NaturalParams::natural());
    // Out-of-range and non-finite weights cannot escape the parameter ranges.
    assert_eq!(scale_params(nominal, 9.0), nominal);
    assert_eq!(scale_params(nominal, f32::NAN), nominal);
}

#[test]
fn all_54_voices_stay_inside_the_envelope_under_every_preset() {
    // D22's bounds are 79 and 323 Hz; the corpus table's two edge medians are
    // 78.9 and 323.1 Hz. The 0.1 Hz is table rounding, not a mapping failure:
    // both edge voices have `w = 0` in the outward direction, so they are
    // left unchanged rather than pushed further out. The strict-bound count is
    // therefore reported separately from the excursions the mapping creates.
    const ROUNDING_HZ: f32 = 0.1;
    let mut pushed_outside = 0usize;
    let mut strict_outside = 0usize;
    let mut minimum = f32::INFINITY;
    let mut maximum = f32::NEG_INFINITY;
    for preset in NaturalPreset::MEASURED {
        let nominal = preset_nominal(preset);
        let mut worst_low = f32::INFINITY;
        let mut worst_high = f32::NEG_INFINITY;
        for f0 in CORPUS_MEDIANS_HZ {
            let weight = headroom_weight(Some(f0), nominal.pitch_st);
            let out = f0 * 2.0f32.powf(nominal.pitch_st * weight / 12.0);
            worst_low = worst_low.min(out);
            worst_high = worst_high.max(out);
            minimum = minimum.min(out);
            maximum = maximum.max(out);
            if out < HEADROOM_LOW_HZ - ROUNDING_HZ || out > HEADROOM_HIGH_HZ + ROUNDING_HZ {
                pushed_outside += 1;
            }
            if !(HEADROOM_LOW_HZ..=HEADROOM_HIGH_HZ).contains(&out) {
                strict_outside += 1;
                // The only strict misses are the unchanged edge voices.
                assert!(
                    (out - f0).abs() < 1e-4,
                    "{} pushed {f0} to {out}, outside the strict bounds",
                    preset.id()
                );
            }
        }
        println!("{}: out {worst_low:.1}-{worst_high:.1} Hz", preset.id());
    }
    println!(
        "54 voices x 4 presets: {pushed_outside} pushed outside (> {ROUNDING_HZ} Hz), \
         {strict_outside} strictly outside, min {minimum:.1} Hz, max {maximum:.1} Hz"
    );
    assert_eq!(
        pushed_outside, 0,
        "no voice may be pushed out of the envelope"
    );
    // The edge voices start outside D22's rounded bounds by 0.1 Hz.
    assert_eq!(strict_outside, 5);
    assert!(minimum >= HEADROOM_LOW_HZ - ROUNDING_HZ);
    assert!(maximum <= HEADROOM_HIGH_HZ + ROUNDING_HZ);
    // The envelope edges are approached, so this is not a vacuous pass.
    assert!(maximum > 320.0, "max {maximum}");
    assert!(minimum < 80.0, "min {minimum}");
}

#[test]
fn middle_keeps_nominal_and_extremes_taper() {
    let median = HEADROOM_REFERENCE_HZ;
    for preset in NaturalPreset::MEASURED {
        let nominal = preset_nominal(preset);
        if nominal.pitch_st == 0.0 {
            continue;
        }
        let weight = headroom_weight(Some(median), nominal.pitch_st);
        let kept = weight;
        assert!(kept >= 0.95, "{} kept {kept}", preset.id());
    }

    // `deep` at the low anchor is at most one semitone.
    assert!(effective(NaturalPreset::Deep, 88.4).abs() <= 1.0);
    // `shift-up` at the 248 Hz stress voice is at most five semitones.
    let up = effective(NaturalPreset::ShiftUp, 248.3);
    assert!(up <= 5.0 && up > 3.0, "shift-up at 248 Hz = {up}");
    // `shift-up` at the highest voice is effectively zero.
    assert!(effective(NaturalPreset::ShiftUp, 323.1).abs() <= 0.05);
}

#[test]
fn estimator_is_sub_percent_on_synthetic_vowels() {
    let mut tracker = F0Tracker::new();
    for f0 in [90.0, 110.0, 150.0, 220.0, 300.0] {
        let input = testutil::vowel(
            f0,
            &[700.0, 1220.0, 2600.0],
            &[80.0, 90.0, 120.0],
            SR,
            24_000,
        );
        let measured = tracker.estimate(&input, SR).expect("voiced vowel");
        let error = 100.0 * (f64::from(measured) - f0) / f0;
        println!("synthetic {f0:.0} Hz -> {measured:.2} Hz ({error:+.2}%)");
        assert!(error.abs() < 1.0, "synthetic {f0}: {measured} Hz");
    }
}

#[test]
fn estimator_is_within_three_percent_of_the_official_fixture_medians() {
    let Some(fixtures) = testutil::load_fixtures() else {
        println!("skipping: fixtures/manifest.json is absent");
        return;
    };
    let mut tracker = F0Tracker::new();
    let mut errors = Vec::new();
    println!("| fixture | measured Hz | ruler Hz | error | voice median |");
    for fixture in &fixtures {
        let Some((_, expected)) = FIXTURE_MEDIANS_HZ
            .iter()
            .find(|(name, _)| *name == fixture.name)
        else {
            continue;
        };
        let measured = tracker
            .estimate(&fixture.samples, SR)
            .expect("fixtures are voiced");
        let error = 100.0 * (f64::from(measured) - expected) / expected;
        let voice = CORPUS_VOICE_MEDIANS_HZ
            .iter()
            .find(|(name, _)| *name == fixture.name)
            .map(|(_, hz)| *hz)
            .unwrap_or(f64::NAN);
        println!(
            "| `{}` | {measured:.2} | {expected:.2} | {error:+.2}% | {voice:.1} |",
            fixture.name
        );
        errors.push(error.abs());
    }
    assert_eq!(errors.len(), 12, "expected all 12 fixtures");
    errors.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
    let median = 0.5 * (errors[5] + errors[6]);
    let worst = errors[11];
    println!("median error {median:.2}%, worst {worst:.2}%");
    assert!(median <= 3.0, "median error {median:.2}%");
    assert!(worst <= 10.0, "worst error {worst:.2}%");
}

#[test]
fn renderer_applies_the_mapping_and_reports_it() {
    let low = testutil::vowel(
        90.0,
        &[700.0, 1220.0, 2600.0],
        &[80.0, 90.0, 120.0],
        SR,
        12_000,
    );
    let mut renderer = NaturalRenderer::d2(SR).expect("valid chain");
    renderer.set_preset(NaturalPreset::Deep);
    let mut mapped = Vec::new();
    let report = renderer
        .process_into_reported(&low, &mut mapped)
        .expect("valid render");
    let measured = report.headroom.measured_f0_hz.expect("voiced vowel");
    let expected_weight = headroom_weight(Some(measured), -4.0);
    assert!((report.headroom.weight - expected_weight).abs() < 1e-6);
    assert!(report.headroom.effective_pitch_st() > -1.0);
    assert!(report.headroom.effective_pitch_st() <= 0.0);
    assert!(report.headroom.effective_formant_ratio() < 1.0);
    assert_eq!(renderer.last_headroom(), Some(report.headroom));

    // The mapped render equals an explicit render at the effective point.
    let mut explicit = NaturalRenderer::d2(SR).expect("valid chain");
    explicit.set_headroom(HeadroomMode::Off);
    explicit.set_params(report.headroom.effective);
    let mut direct = Vec::new();
    explicit
        .process_into(&low, &mut direct)
        .expect("valid render");
    assert_eq!(mapped, direct, "the mapping must be the only difference");

    // Off reproduces the nominal fixed preset for A/B.
    let mut fixed = NaturalRenderer::d2(SR).expect("valid chain");
    fixed.set_headroom(HeadroomMode::Off);
    fixed.set_preset(NaturalPreset::Deep);
    let mut fixed_out = Vec::new();
    let fixed_report = fixed
        .process_into_reported(&low, &mut fixed_out)
        .expect("valid render");
    assert_eq!(fixed_report.headroom.weight, 1.0);
    assert_eq!(
        fixed_report.headroom.effective,
        NaturalPreset::Deep.params()
    );
    assert_ne!(fixed_out, mapped);

    // A zero-nominal-pitch preset keeps the full tilt and needs no measurement.
    let mut bright = NaturalRenderer::d2(SR).expect("valid chain");
    bright.set_preset(NaturalPreset::Bright);
    let mut out = Vec::new();
    let report = bright
        .process_into_reported(&low, &mut out)
        .expect("valid render");
    assert_eq!(report.headroom.weight, 1.0);
    assert_eq!(report.headroom.measured_f0_hz, None);
    assert_eq!(report.headroom.effective, NaturalPreset::Bright.params());
}

#[test]
fn reported_types_are_public_and_parseable() {
    assert_eq!(HeadroomMode::from_id("on"), Some(HeadroomMode::On));
    assert_eq!(HeadroomMode::from_id("off"), Some(HeadroomMode::Off));
    assert_eq!(HeadroomMode::from_id("maybe"), None);
    assert_eq!(HeadroomMode::default(), HeadroomMode::On);
    assert_eq!(HeadroomMode::Off.id(), "off");

    let nominal = NaturalParams::new(7.0, 1.10, 0.75);
    let report = HeadroomReport::new(HeadroomMode::On, nominal, Some(HEADROOM_HIGH_HZ));
    assert_eq!(report.weight, 0.0);
    assert!(report.no_headroom());
    assert_eq!(report.effective, NaturalParams::natural());
    let report = HeadroomReport::new(
        HeadroomMode::On,
        NaturalPreset::Bright.params(),
        Some(200.0),
    );
    assert_eq!(report.weight, 1.0);
    assert!(!report.no_headroom());

    // The aggregate report is constructible by callers (it has no private
    // fields), and the loudness half is still reachable by name.
    fn takes_report(report: NaturalRenderReport) -> f32 {
        report.loudness.source_rms_dbfs + report.headroom.weight
    }
    let _ = takes_report;
}

/// Write the **D22-effective** preset matrix and a manifest for the D17 ruler.
///
/// This is the companion to `params::tests::write_preset_renders`, which writes
/// the nominal matrix with `HeadroomMode::Off`. The manifest carries the
/// effective step/ratio, so `scripts/metrics.py` scores the render against the
/// shift the DSP actually applied.
///
/// ```text
/// cargo test -p rasp-dsp --release write_headroom_renders -- --ignored --nocapture
/// scripts/.venv/bin/python scripts/metrics.py \
///   --renders "${TMPDIR:-/tmp}/rasp-p05b-renders" \
///   --manifest "${TMPDIR:-/tmp}/rasp-p05b-renders/manifest.tsv" --check
/// ```
#[test]
#[ignore = "writes D22-effective preset renders for metrics.py; cargo test -p rasp-dsp --release write_headroom_renders -- --ignored --nocapture"]
fn write_headroom_renders() {
    use crate::params::NaturalPreset;

    let directory = std::env::temp_dir().join("rasp-p05b-renders");
    std::fs::create_dir_all(&directory).expect("create render directory");
    let mut manifest = String::from(
        "fixture\tstep\tcents\tratio\tpreset\ttilt_db_per_oct\tfile\tsamples\tweight\teffective_pitch_st\teffective_formant_ratio\tf0_hz\n",
    );
    let mut written = 0usize;

    let mut render_one = |name: &str, preset: NaturalPreset, input: &[f32]| {
        let mut renderer = NaturalRenderer::d2(SR).expect("valid chain");
        renderer.set_preset(preset);
        renderer.set_headroom(HeadroomMode::On);
        let mut output = Vec::new();
        let report: NaturalRenderReport = renderer
            .process_into_reported(input, &mut output)
            .expect("valid render");
        let headroom = report.headroom;
        let effective = headroom.effective;
        let step = effective.pitch_st.round() as i32;
        let ratio = 2.0f32.powf(effective.pitch_st / 12.0);
        let file = format!("{name}.{}.headroom.f32", preset.id());
        let bytes: Vec<u8> = output.iter().flat_map(|x| x.to_le_bytes()).collect();
        std::fs::write(directory.join(&file), bytes).expect("write render");
        manifest.push_str(&format!(
            "{name}\t{step:+}\t{:.6}\t{ratio:.6}\t{}\t{:.2}\t{file}\t{}\t{:.4}\t{:.4}\t{:.6}\t{}\n",
            effective.pitch_st * 100.0,
            preset.id(),
            effective.tilt_db_per_oct,
            output.len(),
            headroom.weight,
            effective.pitch_st,
            effective.formant_ratio,
            headroom
                .measured_f0_hz
                .map(|hz| format!("{hz:.3}"))
                .unwrap_or_else(|| "null".to_string()),
        ));
        written += 1;
    };

    if let Some(fixtures) = testutil::load_fixtures() {
        for fixture in &fixtures {
            for preset in NaturalPreset::MEASURED {
                render_one(&fixture.name, preset, &fixture.samples);
            }
        }
        println!("rendered {} fixtures", fixtures.len());
    } else {
        println!("fixtures absent; rendering synthetic vowels only");
    }

    let formant_sets: [(&str, [f64; 3], [f64; 3]); 2] = [
        (
            "700/1220/2600",
            [700.0, 1220.0, 2600.0],
            [80.0, 90.0, 120.0],
        ),
        (
            "500/1500/2500",
            [500.0, 1500.0, 2500.0],
            [70.0, 90.0, 120.0],
        ),
    ];
    for (set_index, (_, formants, bandwidths)) in formant_sets.iter().enumerate() {
        for f0 in [90.0f64, 110.0, 150.0, 220.0] {
            let vowel_name = format!("synthetic_f{f0:.0}_v{set_index}");
            let input = testutil::vowel(f0, formants, bandwidths, SR, 24_000);
            let bytes: Vec<u8> = input.iter().flat_map(|x| x.to_le_bytes()).collect();
            std::fs::write(directory.join(format!("{vowel_name}.ref.f32")), bytes)
                .expect("write reference");
            for preset in NaturalPreset::MEASURED {
                render_one(&vowel_name, preset, &input);
            }
        }
    }

    std::fs::write(directory.join("manifest.tsv"), manifest).expect("write manifest");
    println!(
        "{written} renders and manifest.tsv in {}",
        directory.display()
    );
}

#[test]
fn full_headroom_cells_are_bit_exact_and_the_reference_is_within_a_hair() {
    // A cell with **full** headroom in the preset's direction (`w` clamps to 1)
    // must render bit-for-bit the same as the pre-D22 fixed point; this is what
    // keeps P-05's synthetic formant numbers valid where D22 does nothing.
    for (preset, f0) in [
        (NaturalPreset::ShiftUp, 110.0),
        (NaturalPreset::ShiftDown, 220.0),
        (NaturalPreset::Bright, 150.0),
        (NaturalPreset::Deep, 220.0),
    ] {
        let input = testutil::vowel(
            f0,
            &[700.0, 1220.0, 2600.0],
            &[80.0, 90.0, 120.0],
            SR,
            12_000,
        );
        let mut on = NaturalRenderer::d2(SR).expect("valid chain");
        on.set_preset(preset);
        let mut mapped = Vec::new();
        let report = on
            .process_into_reported(&input, &mut mapped)
            .expect("valid render");
        assert_eq!(report.headroom.weight, 1.0, "{} at {f0}", preset.id());
        assert_eq!(report.headroom.effective, preset.params());

        let mut off = NaturalRenderer::d2(SR).expect("valid chain");
        off.set_headroom(HeadroomMode::Off);
        off.set_preset(preset);
        let mut fixed = Vec::new();
        off.process_into(&input, &mut fixed).expect("valid render");
        assert_eq!(mapped, fixed, "{} at {f0} changed", preset.id());
    }

    // At exactly the 178 Hz reference the estimator lands within float error of
    // the reference, so w differs from 1 by <1e-4 and the render is a null.
    let input = testutil::vowel(
        178.0,
        &[700.0, 1220.0, 2600.0],
        &[80.0, 90.0, 120.0],
        SR,
        12_000,
    );
    for preset in NaturalPreset::MEASURED {
        let mut on = NaturalRenderer::d2(SR).expect("valid chain");
        on.set_preset(preset);
        let mut mapped = Vec::new();
        let report = on
            .process_into_reported(&input, &mut mapped)
            .expect("valid render");
        assert!(
            (report.headroom.weight - 1.0).abs() < 1e-4,
            "{} weight {}",
            preset.id(),
            report.headroom.weight
        );
        let mut off = NaturalRenderer::d2(SR).expect("valid chain");
        off.set_headroom(HeadroomMode::Off);
        off.set_preset(preset);
        let mut fixed = Vec::new();
        off.process_into(&input, &mut fixed).expect("valid render");
        let diff = mapped
            .iter()
            .zip(&fixed)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        let peak = fixed.iter().fold(0.0f32, |acc, x| acc.max(x.abs()));
        let null_db = 20.0 * (diff.max(1e-12) / peak.max(1e-12)).log10();
        println!(
            "{} at 178 Hz: w {:.8}, null {null_db:.1} dB",
            preset.id(),
            report.headroom.weight
        );
        assert!(null_db <= -80.0, "{} null {null_db} dB", preset.id());
    }
}
