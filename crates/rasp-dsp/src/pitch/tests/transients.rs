//! Transient-handling measurements (PLAN P-04).
//!
//! Every number in these tests compares the **same render** with
//! [`TransientPolicy::Off`] (P-02 behaviour) and [`TransientPolicy::Reset`]
//! (default). The controlled signals are an impulse in digital silence, an
//! impulse in a −60 dBFS noise bed (so "dB above the preceding floor" is
//! defined), and a synthetic plosive: a 40 ms stop gap, a 5 ms noise burst,
//! then a vowel. The real material is the three clips the P-04 packet names,
//! decoded from `web/audio/` with the workspace decoder.
//!
//! The tables are `#[ignore]`d:
//!
//! ```text
//! cargo test -p rasp-dsp --release transient_tables -- --ignored --nocapture
//! ```
//!
//! The non-ignored tests assert the deltas the tables show, at tolerances well
//! inside the measured values (they are regression gates, not re-derivations).

use super::*;
use crate::testutil;
use crate::transform::{VoiceTransformConfig, VoiceTransformer};

const SR: u32 = 48_000;
/// One second of audio: enough for a stable centroid, short enough for debug.
const LEN: usize = 48_000;

/// Impulse position, one second in: far from the edges.
const IMPULSE_AT: usize = 24_000;

fn make_shifter(policy: TransientPolicy, ratio: f32) -> PitchShifter {
    let mut shifter =
        PitchShifter::new(SR, PitchShiftConfig::auto().with_transients(policy)).expect("valid");
    shifter.set_ratio(ratio);
    shifter
}

fn render(input: &[f32], policy: TransientPolicy, ratio: f32) -> Vec<f32> {
    make_shifter(policy, ratio)
        .process(input)
        .expect("valid render")
}

/// An impulse at [`IMPULSE_AT`] plus a −60 dBFS white-noise bed.
fn impulse_in_noise(noise_db: f32) -> Vec<f32> {
    let amplitude = 10.0f32.powf(noise_db / 20.0);
    let mut samples: Vec<f32> = testutil::white_noise(LEN, 7)
        .into_iter()
        .map(|value| value * amplitude)
        .collect();
    samples[IMPULSE_AT] = 1.0;
    samples
}

/// A synthetic plosive: a 20 ms stop gap after a preceding vowel, 5 ms of
/// deterministic noise, then a 220 Hz vowel. The burst starts at 500 ms.
///
/// The 20 ms gap gives a clean floor in `[onset − 120 ms, onset − 40 ms)`
/// (the preceding vowel) and, in the input, essentially no energy in the
/// 20 ms before the burst. Any energy the vocoder puts there is pre-echo.
fn synthetic_plosive() -> Vec<f32> {
    let mut samples = vec![0.0f32; LEN];
    let burst_at = 24_000;
    let previous = testutil::vowel(
        180.0,
        &[700.0, 1220.0, 2600.0],
        &[80.0, 90.0, 120.0],
        SR,
        burst_at - 960,
    );
    samples[..previous.len()].copy_from_slice(&previous);
    let noise = testutil::white_noise(240, 11);
    let peak = noise.iter().fold(0.0f32, |acc, value| acc.max(value.abs()));
    let scale = if peak > 0.0 { 0.5 / peak } else { 0.0 };
    for (index, value) in noise.iter().enumerate() {
        samples[burst_at + index] = value * scale;
    }
    let vowel = testutil::vowel(
        220.0,
        &[700.0, 1220.0, 2600.0],
        &[80.0, 90.0, 120.0],
        SR,
        LEN - burst_at - 240,
    );
    samples[burst_at + 240..].copy_from_slice(&vowel);
    samples
}

/// RMS of `[from, to)`, or 0 when the range is empty.
fn window_rms(signal: &[f32], from: usize, to: usize) -> f64 {
    let to = to.min(signal.len());
    if from >= to {
        return 0.0;
    }
    let sum: f64 = signal[from..to]
        .iter()
        .map(|&x| (x as f64) * (x as f64))
        .sum();
    (sum / (to - from) as f64).sqrt()
}

/// Energy in the 20 ms before `onset`, in dB above the preceding floor.
///
/// The floor is the RMS of `[onset − 120 ms, onset − 40 ms)`, which is the
/// stop gap / previous-phoneme decay for a real plosive. When that window is
/// digital silence the floor is floored at −140 dBFS so the result stays
/// finite and comparable.
fn pre_echo_db(signal: &[f32], onset: usize) -> f64 {
    let pre = window_rms(signal, onset.saturating_sub(960), onset);
    let floor = window_rms(
        signal,
        onset.saturating_sub(5_760),
        onset.saturating_sub(1_920),
    );
    20.0 * (pre.max(1e-7) / floor.max(1e-7)).log10()
}

/// Median and maximum over a list, ignoring non-finite values.
fn median_max(values: &mut Vec<f64>) -> (f64, f64) {
    values.retain(|value| value.is_finite());
    if values.is_empty() {
        return (f64::NAN, f64::NAN);
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
    (values[values.len() / 2], values[values.len() - 1])
}

/// 1 ms RMS envelope of `|signal|`, one value per millisecond (48 samples).
///
/// `envelope(signal)[j]` is the RMS of samples `[j·48, j·48 + 48)`, so a sample
/// index maps to `j * 48` exactly as the onsets below assume.
fn envelope(signal: &[f32]) -> Vec<f64> {
    const W: usize = 48;
    signal
        .as_chunks::<W>()
        .0
        .iter()
        .map(|chunk| {
            let sum: f64 = chunk.iter().map(|&x| (x as f64) * (x as f64)).sum();
            (sum / W as f64).sqrt()
        })
        .collect()
}

/// Rise time from 10% to 90% of the attack peak, in milliseconds.
///
/// The peak is searched in `[onset − 5 ms, onset + 30 ms)`, so the measurement
/// follows the plosive's attack rather than a louder vowel 100 ms later. `None`
/// when either crossing is missing.
fn rise_time_ms(signal: &[f32], onset: usize) -> Option<f64> {
    let env = envelope(signal);
    if env.is_empty() {
        return None;
    }
    let from = (onset.saturating_sub(5 * 48)) / 48;
    let to = ((onset + 30 * 48) / 48).min(env.len());
    if to <= from + 1 {
        return None;
    }
    let mut peak_index = from;
    let mut peak = 0.0f64;
    for (index, &value) in env.iter().enumerate().take(to).skip(from) {
        if value > peak {
            peak = value;
            peak_index = index;
        }
    }
    if peak <= 0.0 {
        return None;
    }
    let mut low = peak_index;
    while low > from && env[low] > 0.1 * peak {
        low -= 1;
    }
    let mut high = low;
    while high < peak_index && env[high] < 0.9 * peak {
        high += 1;
    }
    if high <= low {
        return None;
    }
    Some((high - low) as f64)
}

/// Strongest 20 ms energy rise after 200 ms, walked back to the attack.
///
/// Returns the sample index of the attack start in the **reference** signal.
/// The 40 ms cap on the walk-back keeps it local instead of drifting into a
/// previous syllable's decay.
fn find_onset(signal: &[f32]) -> Option<usize> {
    let env = envelope(signal);
    if env.len() < 40 {
        return None;
    }
    let mut best = (f64::NEG_INFINITY, 0usize);
    for index in 20..env.len() {
        let sample = index * 48;
        if sample < SR as usize / 5 || env[index] < 0.01 {
            continue;
        }
        let previous = env[index - 20].max(1e-9);
        let rise = 20.0 * (env[index] / previous).log10();
        if rise > best.0 {
            best = (rise, index);
        }
    }
    if !best.0.is_finite() {
        return None;
    }
    let peak_index = best.1;
    let mut index = peak_index;
    while index > peak_index.saturating_sub(40) && env[index] > 0.05 * env[peak_index] {
        index -= 1;
    }
    Some(index * 48)
}

fn centroid(signal: &[f32], position: usize, radius: usize) -> Option<f64> {
    let low = position.saturating_sub(radius);
    let high = (position + radius).min(signal.len());
    let mut weighted = 0.0f64;
    let mut total = 0.0f64;
    for (index, &value) in signal.iter().enumerate().take(high).skip(low) {
        let energy = (value as f64) * (value as f64);
        weighted += index as f64 * energy;
        total += energy;
    }
    (total > 1e-12).then_some(weighted / total)
}

/// The three real clips the P-04 packet names, decoded from the vendored
/// corpus with the workspace decoder.
fn named_clips() -> Option<Vec<(String, Vec<f32>)>> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/audio");
    let mut clips = Vec::new();
    for name in [
        "af_heart/watch-this",
        "am_eric/good-luck",
        "am_onyx/got-time",
    ] {
        let path = root.join(format!("{name}.weba"));
        if !path.is_file() {
            return None;
        }
        let decoded = rasp_opus::decode_weba_file(&path).ok()?;
        clips.push((name.to_string(), decoded.samples));
    }
    Some(clips)
}

// ---------------------------------------------------------------------------
// Regression gates (always run)
// ---------------------------------------------------------------------------

#[test]
fn transient_reset_is_deterministic_and_bypass_is_unchanged() {
    let input = synthetic_plosive();
    let first = render(&input, TransientPolicy::Reset, 0.5);
    let second = render(&input, TransientPolicy::Reset, 0.5);
    assert_eq!(first, second, "the detector must be per-render state");

    // Ratio 1 is the bit-exact bypass: the policy cannot change it.
    let bypass_reset = render(&input, TransientPolicy::Reset, 1.0);
    let bypass_off = render(&input, TransientPolicy::Off, 1.0);
    assert_eq!(bypass_reset, input);
    assert_eq!(bypass_off, input);

    // Silence has no onset and off is always zero resets.
    let mut quiet = make_shifter(TransientPolicy::Reset, 0.5);
    let silence = vec![0.0f32; LEN];
    quiet.process(&silence).expect("valid");
    assert_eq!(quiet.transient_count(), 0);
    let mut off = make_shifter(TransientPolicy::Off, 0.5);
    off.process(&synthetic_plosive()).expect("valid");
    assert_eq!(off.transient_count(), 0);
}

#[test]
fn onsets_are_detected_on_speech_but_not_on_a_steady_tone() {
    let mut shifter = make_shifter(TransientPolicy::Reset, 0.5);
    shifter.process(&synthetic_plosive()).expect("valid");
    assert!(
        shifter.transient_count() >= 1,
        "the synthetic plosive must trigger at least one reset, got {}",
        shifter.transient_count()
    );

    // A tone that starts abruptly has exactly one onset; the steady state
    // must not keep triggering (the test is really the count, not the start).
    let tone = testutil::sine(220.0, SR, LEN);
    let mut steady = make_shifter(TransientPolicy::Reset, 0.5);
    steady.process(&tone).expect("valid");
    assert!(
        steady.transient_count() <= 1,
        "a steady tone must not be treated as a chain of onsets, got {}",
        steady.transient_count()
    );
    let mut silent_start = vec![0.0f32; 4_800];
    silent_start.extend_from_slice(&tone);
    let mut steady = make_shifter(TransientPolicy::Reset, 0.5);
    steady.process(&silent_start).expect("valid");
    assert!(
        steady.transient_count() <= 1,
        "a tone after silence has one attack, got {} resets",
        steady.transient_count()
    );
}

#[test]
fn reset_reduces_pre_echo_on_an_impulse() {
    let input = impulse_in_noise(-60.0);
    let off = pre_echo_db(&render(&input, TransientPolicy::Off, 0.5), IMPULSE_AT);
    let reset = pre_echo_db(&render(&input, TransientPolicy::Reset, 0.5), IMPULSE_AT);
    println!("impulse pre-echo: off {off:.1} dB, reset {reset:.1} dB");
    assert!(
        reset < off - 3.0,
        "phase reset must reduce impulse pre-echo: off {off:.1} dB, reset {reset:.1} dB"
    );
}

#[test]
fn transient_handling_on_the_named_clips() {
    let Some(clips) = named_clips() else {
        println!("skipping: web/audio is absent");
        return;
    };
    let mut pooled_pre = Vec::new();
    let mut pooled_rise = Vec::new();
    for (name, samples) in clips {
        // One Reset render supplies both the onset list and the "after"
        // signal; Off is the "before". The correction pass is covered by the
        // ignored table below, and the authoritative pitch check is
        // `write_p04_ab_renders` + scripts/metrics.py.
        let mut probe = make_shifter(TransientPolicy::Reset, 0.5);
        let reset_render = probe.process(&samples).expect("valid render");
        let onsets: Vec<usize> = probe.transient_samples().to_vec();
        assert!(!onsets.is_empty(), "{name}: no onsets detected");
        let off_render = render(&samples, TransientPolicy::Off, 0.5);

        let mut input_pre = Vec::new();
        let mut off_pre = Vec::new();
        let mut reset_pre = Vec::new();
        let mut off_rise = Vec::new();
        let mut reset_rise = Vec::new();
        for &onset in &onsets {
            input_pre.push(pre_echo_db(&samples, onset));
            off_pre.push(pre_echo_db(&off_render, onset));
            reset_pre.push(pre_echo_db(&reset_render, onset));
            pooled_pre.push(
                reset_pre.last().copied().unwrap_or(0.0) - off_pre.last().copied().unwrap_or(0.0),
            );
            if let (Some(before), Some(after)) = (
                rise_time_ms(&off_render, onset),
                rise_time_ms(&reset_render, onset),
            ) {
                off_rise.push(before);
                reset_rise.push(after);
                pooled_rise.push(after - before);
            }
        }
        let (input_pre, _) = median_max(&mut input_pre);
        let (off_pre, _) = median_max(&mut off_pre);
        let (reset_pre, _) = median_max(&mut reset_pre);
        let (off_rise, _) = median_max(&mut off_rise);
        let (reset_rise, _) = median_max(&mut reset_rise);
        println!(
            "{name}: {} onsets; pre-echo input {input_pre:.1} / off {off_pre:.1} / reset \
             {reset_pre:.1} dB; rise off {off_rise:.0} / reset {reset_rise:.0} ms",
            onsets.len()
        );
    }
    let (pre_median, pre_worst) = median_max(&mut pooled_pre);
    let (rise_median, rise_worst) = median_max(&mut pooled_rise);
    println!(
        "pooled: pre-echo Δ median {pre_median:+.2} dB (worst {pre_worst:+.2}), rise Δ \
         median {rise_median:+.2} ms (worst {rise_worst:+.2})"
    );
    // Per-onset detail is printed above. The gate is pooled because one
    // mid-voiced onset in af_heart has a 20 ms "pre" window that contains the
    // previous syllable's decay; a phase change redistributes that energy in
    // either direction, so that single point is reported rather than claimed.
    assert!(
        pre_median < 0.0,
        "phase reset must reduce pre-echo across the onset set: median Δ {pre_median:+.2} dB"
    );
    assert!(
        rise_median <= 0.0,
        "phase reset must not slow the attack across the onset set: median Δ \
         {rise_median:+.2} ms"
    );
}

// ---------------------------------------------------------------------------
// Measurement tables (ignored)
// ---------------------------------------------------------------------------

/// Pre-echo, centroid drift and rise time, before vs after, for every
/// controlled signal and the three named real clips.
///
/// ```text
/// cargo test -p rasp-dsp --release transient_tables -- --ignored --nocapture
/// ```
#[test]
#[ignore = "measurement table: cargo test -p rasp-dsp --release transient_tables -- --ignored --nocapture"]
fn transient_tables() {
    println!("\n## Pre-echo (20 ms before the onset, dB above the preceding floor)\n");
    println!("| signal | ratio | input | off | reset | Δ (dB) |");
    println!("|---|---|---|---|---|---|");

    let impulse = impulse_in_noise(-60.0);
    for ratio in [0.5f32, 1.0, 2.0] {
        let reference = pre_echo_db(&impulse, IMPULSE_AT);
        let off = pre_echo_db(&render(&impulse, TransientPolicy::Off, ratio), IMPULSE_AT);
        let reset = pre_echo_db(&render(&impulse, TransientPolicy::Reset, ratio), IMPULSE_AT);
        let mut probe = make_shifter(TransientPolicy::Reset, ratio);
        probe.process(&impulse).expect("valid");
        println!(
            "| impulse in −60 dB noise | {ratio} | {reference:.1} | {off:.1} | {reset:.1} | {:+.1} |",
            reset - off
        );
        println!(
            "  resets: {}, at {:?}",
            probe.transient_count(),
            probe.transient_samples()
        );
    }

    let plosive = synthetic_plosive();
    if let Some(onset) = find_onset(&plosive) {
        for ratio in [0.5f32, 1.0, 2.0] {
            let reference = pre_echo_db(&plosive, onset);
            let off = pre_echo_db(&render(&plosive, TransientPolicy::Off, ratio), onset);
            let reset = pre_echo_db(&render(&plosive, TransientPolicy::Reset, ratio), onset);
            println!(
                "| synthetic plosive | {ratio} | {reference:.1} | {off:.1} | {reset:.1} | {:+.1} |",
                reset - off
            );
        }
    }

    if let Some(clips) = named_clips() {
        for (name, samples) in &clips {
            let mut probe = make_shifter(TransientPolicy::Reset, 0.5);
            probe.process(samples).expect("valid");
            let detected: Vec<usize> = probe.transient_samples().to_vec();
            if detected.is_empty() {
                continue;
            }
            for ratio in [0.5f32, 1.0, 2.0] {
                let mut reference = Vec::new();
                let mut off = Vec::new();
                let mut reset = Vec::new();
                for &onset in &detected {
                    reference.push(pre_echo_db(samples, onset));
                    off.push(pre_echo_db(
                        &render(samples, TransientPolicy::Off, ratio),
                        onset,
                    ));
                    reset.push(pre_echo_db(
                        &render(samples, TransientPolicy::Reset, ratio),
                        onset,
                    ));
                }
                let (reference_median, _) = median_max(&mut reference);
                let (off_median, _) = median_max(&mut off);
                let (reset_median, _) = median_max(&mut reset);
                println!(
                    "| {name} ({}) | {ratio} | {reference_median:.1} | {off_median:.1} | \
                     {reset_median:.1} | {:+.1} |",
                    detected.len(),
                    reset_median - off_median
                );
            }
        }
    } else {
        println!("| real clips | — | — | — | — | web/audio absent |");
    }

    println!("\n## Impulse centroid drift (samples; the P-02 test's +4N search radius)\n");
    println!("| ratio | off | reset | Δ |");
    println!("|---|---|---|---|");
    let mut impulse_silent = vec![0.0f32; LEN];
    impulse_silent[IMPULSE_AT] = 1.0;
    for ratio in [0.5f32, 1.0, 2.0] {
        let radius = 4 * make_shifter(TransientPolicy::Reset, ratio).window_size();
        let off_signal = render(&impulse_silent, TransientPolicy::Off, ratio);
        let reset_signal = render(&impulse_silent, TransientPolicy::Reset, ratio);
        let off = centroid(&off_signal, IMPULSE_AT, radius)
            .map(|centroid| centroid - IMPULSE_AT as f64)
            .unwrap_or(f64::NAN);
        let reset = centroid(&reset_signal, IMPULSE_AT, radius)
            .map(|centroid| centroid - IMPULSE_AT as f64)
            .unwrap_or(f64::NAN);
        println!(
            "| {ratio} | {off:+.1} | {reset:+.1} | {:+.1} |",
            reset - off
        );
    }

    println!("\n## Onset rise time (median 10% → 90% of the local peak, ms)\n");
    println!("| signal | ratio | input | off | reset |");
    println!("|---|---|---|---|---|");
    let mut rows: Vec<(String, Vec<usize>, Vec<f32>)> = Vec::new();
    let mut probe = make_shifter(TransientPolicy::Reset, 0.5);
    probe.process(&plosive).expect("valid");
    let detected: Vec<usize> = probe.transient_samples().to_vec();
    rows.push(("synthetic plosive".to_string(), detected, plosive.clone()));
    if let Some(clips) = named_clips() {
        for (name, samples) in clips {
            let mut probe = make_shifter(TransientPolicy::Reset, 0.5);
            probe.process(&samples).expect("valid");
            let detected: Vec<usize> = probe.transient_samples().to_vec();
            if !detected.is_empty() {
                rows.push((name, detected, samples));
            }
        }
    }
    for (name, onsets, reference) in rows {
        let mut input_rise = Vec::new();
        for &onset in &onsets {
            if let Some(value) = rise_time_ms(&reference, onset) {
                input_rise.push(value);
            }
        }
        let (input_median, _) = median_max(&mut input_rise);
        for ratio in [0.5f32, 1.0, 2.0] {
            let off_render = render(&reference, TransientPolicy::Off, ratio);
            let reset_render = render(&reference, TransientPolicy::Reset, ratio);
            let mut off = Vec::new();
            let mut reset = Vec::new();
            for &onset in &onsets {
                if let Some(value) = rise_time_ms(&off_render, onset) {
                    off.push(value);
                }
                if let Some(value) = rise_time_ms(&reset_render, onset) {
                    reset.push(value);
                }
            }
            let (off_median, _) = median_max(&mut off);
            let (reset_median, _) = median_max(&mut reset);
            println!(
                "| {name} ({}) | {ratio} | {input_median:.2} | {off_median:.2} | {reset_median:.2} |",
                onsets.len()
            );
        }
    }

    println!("\n## Transient count (detected onsets per render)\n");
    println!("| signal | ratio | onsets |");
    println!("|---|---|---|");
    for (name, samples) in named_clips().unwrap_or_default() {
        for ratio in [0.5f32, 1.0, 2.0] {
            let mut probe = make_shifter(TransientPolicy::Reset, ratio);
            probe.process(&samples).expect("valid");
            println!("| {name} | {ratio} | {} |", probe.transient_count());
        }
    }

    // The correction pass keeps every bin's phase, so it cannot add a precursor
    // of its own: the transformer's pre-echo must follow the shifter's reset.
    println!("\n## Transformer output at −12 st, formant 1.0 (median pre-echo, dB)\n");
    println!("| signal | shifter off | shifter reset | transformer off | transformer reset |");
    println!("|---|---|---|---|---|");
    if let Some(clips) = named_clips() {
        let make = |policy: TransientPolicy| {
            let mut transformer = VoiceTransformer::new(
                SR,
                VoiceTransformConfig {
                    pitch: PitchShiftConfig::auto().with_transients(policy),
                    envelope: crate::envelope::EnvelopeConfig::cepstral(),
                    correction_window: crate::transform::CorrectionWindowPolicy::Auto,
                },
            )
            .expect("valid transformer");
            transformer.set_pitch_semitones(-12.0);
            transformer.set_formant_ratio(1.0);
            transformer
        };
        for (name, samples) in clips {
            let mut probe = make_shifter(TransientPolicy::Reset, 0.5);
            probe.process(&samples).expect("valid");
            let onsets: Vec<usize> = probe.transient_samples().to_vec();
            if onsets.is_empty() {
                continue;
            }
            let shifter_off = render(&samples, TransientPolicy::Off, 0.5);
            let shifter_reset = render(&samples, TransientPolicy::Reset, 0.5);
            let transformer_off = make(TransientPolicy::Off)
                .process(&samples)
                .expect("valid render");
            let transformer_reset = make(TransientPolicy::Reset)
                .process(&samples)
                .expect("valid render");
            let median = |signal: &[f32]| {
                let mut values: Vec<f64> = onsets
                    .iter()
                    .map(|&onset| pre_echo_db(signal, onset))
                    .collect();
                median_max(&mut values).0
            };
            println!(
                "| {name} | {:.1} | {:.1} | {:.1} | {:.1} |",
                median(&shifter_off),
                median(&shifter_reset),
                median(&transformer_off),
                median(&transformer_reset)
            );
        }
    }

    // P-02 frame-indexed pitch: paired per-frame ratios on the shared 40 ms
    // grid, framed outside the reset regions. The authoritative check is
    // metrics.py (see `write_p04_ab_renders`); this is the same statistic on
    // the three real clips so the report can name the window and frame policy.
    println!(
        "\n## Frame-indexed pitch outside transients (40 ms frames, 20 ms hop, 80–550 Hz band)\n"
    );
    println!("| signal | ratio | frames | median |Δ| c | p90 |Δ| c | >100 c |");
    println!("|---|---|---|---|---|---|");
    if let Some(clips) = named_clips() {
        for (name, samples) in clips {
            let input_f0 = testutil::estimate_f0(&samples, SR, 60.0, 400.0).unwrap_or(f64::NAN);
            for ratio in [0.5f32, 2.0] {
                let expected = input_f0 * f64::from(ratio);
                if !(80.0..=550.0).contains(&expected) {
                    println!("| {name} | {ratio} | — | N/A | N/A | N/A |");
                    continue;
                }
                let mut probe = make_shifter(TransientPolicy::Reset, ratio);
                let reset_render = probe.process(&samples).expect("valid render");
                let onsets: Vec<usize> = probe.transient_samples().to_vec();
                let off_render = render(&samples, TransientPolicy::Off, ratio);
                let frame = 1_920usize;
                let hop = 960usize;
                let mut deltas = Vec::new();
                let mut start = 0;
                while start + frame <= samples.len() {
                    let centre = start + frame / 2;
                    let near_onset = onsets
                        .iter()
                        .any(|&onset| (centre as i64 - onset as i64).abs() <= 1_536);
                    if !near_onset {
                        if let (Some(a), Some(b)) = (
                            testutil::estimate_f0(
                                &off_render[start..start + frame],
                                SR,
                                60.0,
                                400.0,
                            ),
                            testutil::estimate_f0(
                                &reset_render[start..start + frame],
                                SR,
                                60.0,
                                400.0,
                            ),
                        ) {
                            deltas.push((1200.0 * (b / a).log2()).abs());
                        }
                    }
                    start += hop;
                }
                let frames = deltas.len();
                let outliers = deltas.iter().filter(|&&value| value > 100.0).count();
                let (median, p90) = median_max(&mut deltas);
                println!("| {name} | {ratio} | {frames} | {median:.2} | {p90:.2} | {outliers} |");
            }
        }
    }

    println!("\n## CPU: milliseconds per second of audio (release build)\n");
    println!("| ratio | off | reset | Δ |");
    println!("|---|---|---|---|");
    let seconds = 5usize;
    let long = testutil::harmonic_vowel(120.0, SR, SR as usize * seconds);
    // Ratio 1 is a bit-exact bypass and would divide by zero here; it is also
    // the one ratio where the transient path does not run at all.
    for ratio in [0.5f32, 0.75, 1.5, 2.0] {
        let mut times = [f64::INFINITY; 2];
        for (index, policy) in [TransientPolicy::Off, TransientPolicy::Reset]
            .into_iter()
            .enumerate()
        {
            let mut probe = make_shifter(policy, ratio);
            let _ = probe.process(&long[..16_384]).expect("warm-up");
            for _ in 0..3 {
                let start = std::time::Instant::now();
                let output = probe.process(&long).expect("render");
                let elapsed = start.elapsed().as_secs_f64();
                std::hint::black_box(&output);
                times[index] = times[index].min(elapsed * 1e3 / seconds as f64);
            }
        }
        println!(
            "| {ratio} | {:.2} | {:.2} | {:+.1}% |",
            times[0],
            times[1],
            100.0 * (times[1] - times[0]) / times[0].max(1e-9)
        );
    }
}

/// Write the P-02 acceptance matrix twice — with onset reset off and on — so
/// `scripts/metrics.py` can score both and the P-04 claim "pitch medians are
/// unchanged" is judged by the project's official ruler rather than by the
/// test-only autocorrelation tracker.
///
/// ```text
/// cargo test -p rasp-dsp --release write_p04_ab_renders -- --ignored --nocapture
/// scripts/.venv/bin/python scripts/metrics.py --renders /tmp/rasp-p04/off   --check
/// scripts/.venv/bin/python scripts/metrics.py --renders /tmp/rasp-p04/reset --check
/// ```
///
/// Output goes to the temp dir, never into the repository.
#[test]
#[ignore = "writes A/B renders for metrics.py; cargo test -p rasp-dsp --release write_p04_ab_renders -- --ignored --nocapture"]
fn write_p04_ab_renders() {
    let Some(fixtures) = crate::testutil::load_fixtures() else {
        println!("fixtures absent; nothing to render");
        return;
    };
    let steps = [-12i32, -7, -4, -1, 0, 1, 4, 7, 12];
    let names = [
        "am_onyx__got-time",
        "af_heart__friends-now",
        "af_aoede__you-got-me",
        "bf_alice__maybe-next-time",
    ];
    let write_f32 = |path: &std::path::Path, samples: &[f32]| {
        let mut bytes = Vec::with_capacity(samples.len() * 4);
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        std::fs::write(path, bytes).expect("write render");
    };
    let root = std::env::temp_dir().join("rasp-p04");
    for (label, policy) in [
        ("off", TransientPolicy::Off),
        ("reset", TransientPolicy::Reset),
    ] {
        let directory = root.join(label);
        std::fs::create_dir_all(&directory).expect("create render directory");
        let mut manifest = String::from("fixture\tstep\tcents\tratio\twindow\thop\tsamples\n");
        let mut shifter = make_shifter(policy, 1.0);
        for fixture in fixtures
            .iter()
            .filter(|fixture| names.contains(&fixture.name.as_str()))
        {
            for step in steps {
                let ratio = 2.0f32.powf(step as f32 / 12.0);
                shifter.set_ratio(ratio);
                let output = shifter.process(&fixture.samples).expect("render");
                assert_eq!(output.len(), fixture.samples.len());
                write_f32(
                    &directory.join(format!("{}.{step:+03}.f32", fixture.name)),
                    &output,
                );
                manifest.push_str(&format!(
                    "{}\t{step:+}\t{}\t{ratio:.6}\t{}\t{}\t{}\n",
                    fixture.name,
                    step * 100,
                    shifter.window_size(),
                    shifter.hop(),
                    output.len()
                ));
            }
        }
        std::fs::write(directory.join("manifest.tsv"), manifest).expect("write manifest");
        println!("{label} renders in {}", directory.display());
    }
}
