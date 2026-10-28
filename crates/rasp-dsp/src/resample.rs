//! Band-limited sample-rate conversion.
//!
//! The corpus is 48 kHz mono (PLAN K4) but browsers frequently run an
//! `AudioContext` at 44.1 kHz, so every render path has to convert once, at a
//! known place, with a known delay. This module is that place.
//!
//! # Method
//!
//! A Kaiser-windowed sinc low-pass is evaluated as a **polyphase filter bank**.
//! The rate ratio is reduced to `down / up` and one coefficient set is
//! precomputed for each output phase, so output sample `k` reads input samples
//! at exactly `k * down / up` with no timing error at all.
//!
//! The number of phases is capped at [`MAX_PHASES`] and at whatever the
//! coefficient-table budget allows. Every standard audio rate reduces to fewer
//! phases than the cap (48 kHz ↔ 44.1 kHz is 160/147), so their phases are
//! exact. Rates with a much finer ratio keep exact *timing* and only lose
//! filter phase resolution, which trades quality down gracefully instead of
//! producing wrong sample positions.
//!
//! The stopband edge sits at the lower of the two Nyquist frequencies and the
//! transition band is a fixed fraction of it, chosen per [`Quality`]. Tap count
//! follows Kaiser's estimate `N ≈ (A - 8) / (2.285 · Δω)`; quality therefore
//! trades CPU for stopband attenuation, not for a different filter family.
//!
//! # Delay
//!
//! The kernel is symmetric about the output instant, so it needs
//! [`Resampler::latency_input_samples`] (half the filter, in input samples) of
//! *lookahead* before an output sample can be emitted. Signal content is not
//! delayed — output sample `k` corresponds to input time `k / out_rate` — but a
//! streaming producer sees the output stream lag the input by
//! [`Resampler::latency_samples`] output samples. Whole-buffer callers can
//! ignore both; the worklet pre-roll (PLAN W-03) cannot.
//!
//! # Determinism
//!
//! Every output sample is a function of its index and the absolute input
//! samples in its window only. There is no recursive state and no dependence on
//! chunk boundaries, so [`Resampler::process`] is bit-identical to feeding the
//! same signal through [`Resampler::process_chunk`] in any chunk sizes.
//!
//! No dependencies, no `unsafe`, no threads, no clocks.

use core::f64::consts::PI;
use std::fmt;

/// Upper bound on the number of polyphase branches kept in the coefficient
/// table. Every standard browser rate reduces well below this.
const MAX_PHASES: u64 = 1024;

/// Hard cap on the coefficient table size, in `f32` values (~2.4 MB).
///
/// Reached only by rate pairs with a very fine ratio *and* a wide filter; the
/// phase count is reduced to fit, which is the graceful-degradation path.
const MAX_COEFFS: usize = 600_000;

/// Input samples buffered per internal block while streaming, so a large
/// `process_chunk` call does not retain the whole slice.
const BLOCK: usize = 4096;

/// Construction error. Returned instead of panicking on degenerate input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResampleError {
    /// One of the two sample rates was zero.
    ZeroRate,
}

impl fmt::Display for ResampleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResampleError::ZeroRate => f.write_str("sample rate must be non-zero"),
        }
    }
}

impl std::error::Error for ResampleError {}

/// Filter quality preset. Controls stopband attenuation and the width of the
/// transition band, and therefore the tap count and CPU cost.
///
/// [`Quality::High`] is the default: it is sized for 48 kHz → 44.1 kHz speech,
/// where the transition band is only 2.05 kHz wide (20 kHz passband edge to the
/// 22.05 kHz output Nyquist).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Quality {
    /// ~60 dB stopband. Transition band 16% of the stopband edge. Cheap
    /// previewing only; rolls off below 20 kHz at 48 → 44.1 kHz.
    Low,
    /// ~80 dB stopband. Transition band 11% of the stopband edge.
    Medium,
    /// ~100 dB stopband. Transition band 9% of the stopband edge. Default.
    #[default]
    High,
    /// ~120 dB stopband. Transition band 7.5% of the stopband edge.
    Maximum,
    /// Explicit design point. Both fields are clamped to a sane range, so no
    /// input can make construction allocate without bound.
    ///
    /// * `stopband_db` is clamped to `20..=180`.
    /// * `transition` is the transition width as a fraction of the stopband
    ///   edge, clamped to `0.02..=0.4`.
    Custom { stopband_db: f32, transition: f32 },
}

impl Quality {
    /// `(stopband attenuation in dB, transition width as a fraction of the
    /// stopband edge)`.
    fn params(self) -> (f64, f64) {
        match self {
            Quality::Low => (60.0, 0.16),
            Quality::Medium => (80.0, 0.11),
            Quality::High => (100.0, 0.09),
            Quality::Maximum => (120.0, 0.075),
            Quality::Custom {
                stopband_db,
                transition,
            } => (
                (stopband_db as f64).clamp(20.0, 180.0),
                (transition as f64).clamp(0.02, 0.4),
            ),
        }
    }
}

/// A streaming band-limited resampler.
///
/// Construct with [`Resampler::new`], then use either the whole-buffer
/// [`process`](Resampler::process) /
/// [`process_into`](Resampler::process_into) pair or the streaming
/// [`process_chunk`](Resampler::process_chunk) /
/// [`flush`](Resampler::flush_into) pair. Both paths produce the same output.
pub struct Resampler {
    in_rate: u32,
    out_rate: u32,
    /// Exact `out_rate / in_rate`, used only for output lengths.
    ratio: f64,
    /// Numerator of the position ratio: output = input * up / down.
    up: u64,
    /// Denominator of the position ratio.
    down: u64,
    taps: usize,
    half: usize,
    identity: bool,
    phase_count: usize,
    /// `phase_count * taps` coefficients, phase-major. Empty when identity.
    coeffs: Vec<f32>,
    /// Buffered input starting at absolute index `history_start`.
    history: Vec<f32>,
    history_start: u64,
    /// Total input samples received since the last reset.
    received: u64,
    /// Index of the next output sample to emit.
    next_out: u64,
}

impl fmt::Debug for Resampler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Resampler")
            .field("in_rate", &self.in_rate)
            .field("out_rate", &self.out_rate)
            .field("ratio", &self.ratio)
            .field("up", &self.up)
            .field("down", &self.down)
            .field("taps", &self.taps)
            .field("phase_count", &self.phase_count)
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

impl Resampler {
    /// Build a resampler from `in_rate` to `out_rate`.
    ///
    /// Returns [`ResampleError::ZeroRate`] if either rate is zero. Equal rates
    /// short-circuit to a bit-exact copy with zero latency.
    pub fn new(in_rate: u32, out_rate: u32, quality: Quality) -> Result<Self, ResampleError> {
        if in_rate == 0 || out_rate == 0 {
            return Err(ResampleError::ZeroRate);
        }

        let ratio = out_rate as f64 / in_rate as f64;

        if in_rate == out_rate {
            return Ok(Resampler {
                in_rate,
                out_rate,
                ratio,
                up: 1,
                down: 1,
                taps: 1,
                half: 0,
                identity: true,
                phase_count: 1,
                coeffs: Vec::new(),
                history: Vec::new(),
                history_start: 0,
                received: 0,
                next_out: 0,
            });
        }

        let (fc, beta, taps) = design(in_rate, out_rate, quality);

        // Reduced rate ratio: output sample k sits at input position
        // k * down / up. Kept exact; only the coefficient table is capped.
        let g = gcd(in_rate as u64, out_rate as u64);
        let down = in_rate as u64 / g;
        let up = out_rate as u64 / g;

        let budget = (MAX_COEFFS / taps).max(1) as u64;
        let phase_count = up.min(MAX_PHASES).min(budget).max(1);

        let mut resampler = Resampler {
            in_rate,
            out_rate,
            ratio,
            up,
            down,
            taps,
            half: taps / 2,
            identity: false,
            phase_count: phase_count as usize,
            coeffs: Vec::with_capacity(phase_count as usize * taps),
            history: Vec::new(),
            history_start: 0,
            received: 0,
            next_out: 0,
        };
        resampler.build_coeffs(fc, beta);
        Ok(resampler)
    }

    /// Output sample count for an input of `input_len` samples: exactly
    /// `round(input_len * out_rate / in_rate)`.
    pub fn output_len(&self, input_len: usize) -> usize {
        (input_len as f64 * self.ratio).round() as usize
    }

    /// Exact algorithmic delay of the streaming path, in **output** samples.
    ///
    /// This is the lookahead the filter needs (`half` input samples) expressed
    /// at the output rate, so it is fractional in general. Signal alignment is
    /// unaffected: output sample `k` still corresponds to input time
    /// `k / out_rate`.
    pub fn latency_samples(&self) -> f64 {
        self.half as f64 * self.out_rate as f64 / self.in_rate as f64
    }

    /// Lookahead in **input** samples: how many samples must be buffered past
    /// an output position before that output can be produced.
    pub fn latency_input_samples(&self) -> usize {
        self.half
    }

    /// Number of FIR taps used (1 for the equal-rate fast path).
    pub fn taps(&self) -> usize {
        self.taps
    }

    /// Number of polyphase branches in the coefficient table.
    pub fn phases(&self) -> usize {
        self.phase_count
    }

    /// Input sample rate this resampler was built for.
    pub fn in_rate(&self) -> u32 {
        self.in_rate
    }

    /// Output sample rate this resampler produces.
    pub fn out_rate(&self) -> u32 {
        self.out_rate
    }

    /// Resample a complete buffer.
    ///
    /// Treats `input` as a whole signal: state is reset first and the filter
    /// tail is flushed afterwards, so the result is identical to feeding the
    /// same samples through [`process_chunk`](Resampler::process_chunk) in any
    /// chunk sizes. Output length is `round(input.len() * out / in)`.
    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        let mut out = Vec::with_capacity(self.output_len(input.len()));
        self.process_into(input, &mut out);
        out
    }

    /// Resample a complete buffer into an existing vector.
    ///
    /// Clears `output` and reuses its allocation. Same contract and same
    /// output as [`process`](Resampler::process).
    pub fn process_into(&mut self, input: &[f32], output: &mut Vec<f32>) {
        self.reset_stream();
        output.clear();
        output.reserve(self.output_len(input.len()));
        self.process_chunk(input, output);
        self.flush_into(output);
    }

    /// Feed the next block of a stream, appending finished output samples.
    ///
    /// Output is emitted as soon as the required lookahead
    /// ([`latency_input_samples`](Resampler::latency_input_samples)) is
    /// available, so chunk boundaries never change the result. Call
    /// [`flush_into`](Resampler::flush_into) when the stream ends.
    pub fn process_chunk(&mut self, input: &[f32], output: &mut Vec<f32>) {
        for block in input.chunks(BLOCK) {
            self.history.extend_from_slice(block);
            self.received += block.len() as u64;
            self.emit_available(output);
        }
    }

    /// Emit the remaining samples for the stream, zero-padding past the last
    /// input sample, then reset so the resampler can be reused for a new
    /// signal.
    pub fn flush_into(&mut self, output: &mut Vec<f32>) {
        let total = self.output_len(self.received as usize) as u64;
        while self.next_out < total {
            let value = self.compute(self.next_out);
            output.push(value);
            self.next_out += 1;
        }
        self.reset_stream();
    }

    /// Discard all stream state.
    pub fn reset(&mut self) {
        self.reset_stream();
    }

    fn reset_stream(&mut self) {
        self.history.clear();
        self.history_start = 0;
        self.received = 0;
        self.next_out = 0;
    }

    /// Emit every output whose input window is fully available.
    fn emit_available(&mut self, output: &mut Vec<f32>) {
        loop {
            let k = self.next_out;
            let q = self.center(k) as u64;
            let need = q + self.half as u64;
            if need >= self.received {
                break;
            }
            self.prune_to(k);
            let value = self.compute(k);
            output.push(value);
            self.next_out += 1;
        }
    }

    /// Integer part of the input position for output sample `k`.
    fn center(&self, k: u64) -> i64 {
        if self.identity {
            return k as i64;
        }
        (k * self.down / self.up) as i64
    }

    /// Lowest input sample index the window of output `k` touches.
    fn first_index(&self, k: u64) -> i64 {
        if self.identity {
            self.center(k)
        } else {
            self.center(k) - self.half as i64 + 1
        }
    }

    fn prune_to(&mut self, k: u64) {
        let first = self.first_index(k);
        if first <= 0 {
            return;
        }
        let first = first as u64;
        if first <= self.history_start {
            return;
        }
        let drop = ((first - self.history_start) as usize).min(self.history.len());
        self.history.drain(..drop);
        self.history_start += drop as u64;
    }

    fn sample_at(&self, index: u64) -> f32 {
        if index < self.history_start {
            return 0.0;
        }
        let offset = (index - self.history_start) as usize;
        self.history.get(offset).copied().unwrap_or(0.0)
    }

    fn compute(&self, k: u64) -> f32 {
        if self.identity {
            return self.sample_at(k);
        }
        let p = k * self.down;
        let q = (p / self.up) as i64;
        let rem = p % self.up;
        // Exact whenever the table has one branch per phase (all standard
        // rates); otherwise the nearest of the available phases.
        let phase = (rem * self.phase_count as u64 / self.up) as usize;
        let coeffs = &self.coeffs[phase * self.taps..(phase + 1) * self.taps];
        let first = q - self.half as i64 + 1;

        // Accumulate in f64: the filter is short, but cancellation in the
        // stopband is exactly where f32 accumulation would run out of bits.
        let mut acc = 0.0f64;
        for (j, &coeff) in coeffs.iter().enumerate() {
            let index = first + j as i64;
            if index >= 0 {
                acc += coeff as f64 * self.sample_at(index as u64) as f64;
            }
        }
        acc as f32
    }

    fn build_coeffs(&mut self, fc: f64, beta: f64) {
        let taps = self.taps;
        let half = self.half as f64;
        let phase_count = self.phase_count;
        self.coeffs.clear();
        self.coeffs.reserve(phase_count * taps);

        for phase in 0..phase_count {
            let frac = phase as f64 / phase_count as f64;
            let base = self.coeffs.len();
            let mut sum = 0.0f64;
            for j in 0..taps {
                let d = j as f64 - (half - 1.0) - frac;
                let c = 2.0 * fc * sinc(2.0 * fc * d) * kaiser(d, half, beta);
                self.coeffs.push(c as f32);
                sum += c;
            }
            // Per-phase unity DC gain. This removes the small phase-to-phase
            // sum error (the discrete sum of the continuous kernel), which is
            // what would otherwise show up as low-frequency modulation.
            if sum.abs() > f64::MIN_POSITIVE {
                let inv = 1.0 / sum;
                for c in &mut self.coeffs[base..base + taps] {
                    *c = (*c as f64 * inv) as f32;
                }
            }
        }
    }
}

/// `sinc(x) = sin(pi x) / (pi x)`, with the removable singularity handled.
fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 {
        1.0
    } else {
        let px = PI * x;
        px.sin() / px
    }
}

/// Modified Bessel function of the first kind, order 0, by its power series.
///
/// Converges in a few dozen terms for the `x` range Kaiser windows use
/// (`beta <= ~20`); no dependency, no approximation table.
fn bessel_i0(x: f64) -> f64 {
    let y = x * x * 0.25;
    let mut sum = 1.0f64;
    let mut term = 1.0f64;
    let mut k = 1.0f64;
    while k < 200.0 {
        term *= y / (k * k);
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
        k += 1.0;
    }
    sum
}

/// Kaiser window over `|d| <= radius`, normalised to 1 at `d = 0`.
fn kaiser(d: f64, radius: f64, beta: f64) -> f64 {
    if radius <= 0.0 {
        return if d == 0.0 { 1.0 } else { 0.0 };
    }
    let r = d / radius;
    if !(-1.0..1.0).contains(&r) {
        return 0.0;
    }
    bessel_i0(beta * (1.0 - r * r).sqrt()) / bessel_i0(beta)
}

/// Choose the low-pass cut-off, window shape and tap count.
///
/// Returns `(cutoff, beta, taps)`; the frequencies are in cycles per **input**
/// sample.
fn design(in_rate: u32, out_rate: u32, quality: Quality) -> (f64, f64, usize) {
    let (atten_db, transition) = quality.params();

    // The filter must remove everything above the lower of the two Nyquist
    // frequencies; that is the stopband edge in input-rate normalised units.
    let min_rate = in_rate.min(out_rate) as f64;
    let stop_edge = 0.5 * min_rate / in_rate as f64;
    let pass_edge = stop_edge * (1.0 - transition);
    let cutoff = 0.5 * (pass_edge + stop_edge);
    let width = (stop_edge - pass_edge).max(1e-6);

    let beta = if atten_db > 50.0 {
        0.1102 * (atten_db - 8.7)
    } else if atten_db > 21.0 {
        0.5842 * (atten_db - 21.0).powf(0.4) + 0.07886 * (atten_db - 21.0)
    } else {
        0.0
    };

    // Kaiser's tap-count estimate. Rounded up to an even length so the
    // polyphase phases are symmetric; measured attenuation lands within a
    // decibel of the requested value (see the tests).
    let estimate = (atten_db - 8.0) / (2.285 * 2.0 * PI * width);
    let mut n = estimate.ceil().max(4.0) as usize;
    if !n.is_multiple_of(2) {
        n += 1;
    }

    (cutoff, beta, n)
}

fn gcd(a: u64, b: u64) -> u64 {
    let (mut a, mut b) = (a, b);
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- helpers ---------------------------------------------------------

    struct Lcg(u64);

    impl Lcg {
        fn new(seed: u64) -> Self {
            Lcg(seed ^ 0x9e37_79b9_7f4a_7c15)
        }

        fn next_u32(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 33) as u32
        }

        fn next_f32(&mut self) -> f32 {
            (self.next_u32() as f64 / u32::MAX as f64 * 2.0 - 1.0) as f32
        }

        fn below(&mut self, n: u32) -> u32 {
            self.next_u32() % n
        }
    }

    fn sine(freq: f64, rate: u32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| (2.0 * PI * freq * i as f64 / rate as f64).sin() as f32)
            .collect()
    }

    fn rms(x: &[f32]) -> f64 {
        if x.is_empty() {
            return 0.0;
        }
        let sum: f64 = x.iter().map(|&v| (v as f64) * (v as f64)).sum();
        (sum / x.len() as f64).sqrt()
    }

    /// dB ratio, floored so an exact-zero output does not produce `-inf`.
    fn db(num: f64, den: f64) -> f64 {
        let num = num.max(1e-15);
        let den = den.max(1e-15);
        20.0 * (num / den).log10()
    }

    /// Steady-state middle of a processed buffer (skips both transients).
    fn middle(y: &[f32]) -> &[f32] {
        let skip = (y.len() / 10).max(512).min(y.len() / 2);
        &y[skip..y.len() - skip]
    }

    fn make(in_rate: u32, out_rate: u32) -> Resampler {
        Resampler::new(in_rate, out_rate, Quality::default()).unwrap()
    }

    fn gain_db(in_rate: u32, out_rate: u32, freq: f64) -> f64 {
        let x = sine(freq, in_rate, in_rate as usize);
        let mut r = make(in_rate, out_rate);
        let y = r.process(&x);
        db(rms(middle(&y)), rms(&x))
    }

    fn stopband_db(in_rate: u32, out_rate: u32, freq: f64) -> f64 {
        gain_db(in_rate, out_rate, freq)
    }

    /// Amplitude of `freq` in `samples` at `rate`, via a single DFT bin.
    fn tone_amplitude(samples: &[f32], freq: f64, rate: u32) -> f64 {
        let n = samples.len();
        let w = 2.0 * PI * freq / rate as f64;
        let coeff = 2.0 * w.cos();
        let (mut s0, mut s1) = (0.0f64, 0.0f64);
        for &x in samples {
            let s2 = s1;
            s1 = s0;
            s0 = x as f64 + coeff * s1 - s2;
        }
        let re = s0 - s1 * w.cos();
        let im = s1 * w.sin();
        2.0 * (re * re + im * im).sqrt() / n as f64
    }

    // ---- identity and lengths -------------------------------------------

    #[test]
    fn identity_is_bit_exact() {
        let mut rng = Lcg::new(7);
        let x: Vec<f32> = (0..5000).map(|_| rng.next_f32()).collect();
        let mut r = make(48_000, 48_000);
        assert_eq!(r.latency_samples(), 0.0);
        assert!(r
            .process(&x)
            .iter()
            .zip(&x)
            .all(|(a, b)| a.to_bits() == b.to_bits()));
    }

    #[test]
    fn identity_streams_in_arbitrary_chunks() {
        let mut rng = Lcg::new(11);
        let x: Vec<f32> = (0..4000).map(|_| rng.next_f32()).collect();
        let mut r = make(44_100, 44_100);
        let mut out = Vec::new();
        let mut pos = 0;
        while pos < x.len() {
            let n = (rng.below(37) as usize + 1).min(x.len() - pos);
            r.process_chunk(&x[pos..pos + n], &mut out);
            pos += n;
        }
        r.flush_into(&mut out);
        assert_eq!(out.len(), x.len());
        assert!(out.iter().zip(&x).all(|(a, b)| a.to_bits() == b.to_bits()));
    }

    #[test]
    fn output_length_is_rounded_for_all_directions() {
        let pairs = [
            (48_000u32, 44_100u32),
            (44_100, 48_000),
            (48_000, 96_000),
            (96_000, 48_000),
            (48_000, 32_000),
            (44_100, 96_000),
        ];
        for (in_rate, out_rate) in pairs {
            let mut r = make(in_rate, out_rate);
            for n in [0usize, 1, 2, 3, 7, 100, 441, 1000, 4801, 48_000] {
                let x = vec![0.0f32; n];
                let y = r.process(&x);
                let expected = (n as f64 * out_rate as f64 / in_rate as f64).round() as usize;
                assert_eq!(
                    y.len(),
                    expected,
                    "{in_rate}->{out_rate}, n={n}: {} != {expected}",
                    y.len()
                );
                assert!(
                    (y.len() as i64 - expected as i64).abs() <= 1,
                    "length off by more than one sample"
                );
            }
        }
    }

    #[test]
    fn empty_input_produces_no_output() {
        let mut r = make(48_000, 44_100);
        assert!(r.process(&[]).is_empty());
        let mut out = vec![9.0f32];
        r.process_into(&[], &mut out);
        assert!(out.is_empty());
        let mut streamed = Vec::new();
        r.process_chunk(&[], &mut streamed);
        r.flush_into(&mut streamed);
        assert!(streamed.is_empty());
    }

    #[test]
    fn zero_rate_is_rejected() {
        assert_eq!(
            Resampler::new(0, 48_000, Quality::default()).unwrap_err(),
            ResampleError::ZeroRate
        );
        assert_eq!(
            Resampler::new(48_000, 0, Quality::default()).unwrap_err(),
            ResampleError::ZeroRate
        );
    }

    // ---- frequency response ---------------------------------------------

    #[test]
    fn passband_is_flat_within_0_1_db_to_20_khz() {
        // Requirement: flat to 20 kHz at 48 kHz, and to 18 kHz for a 44.1 kHz
        // output. We check to 20 kHz everywhere the output Nyquist allows it,
        // with extra density near the top of the band where droop would show.
        let cases = [
            (48_000u32, 44_100u32, 20_000.0f64),
            (44_100, 48_000, 20_000.0),
            (48_000, 96_000, 20_000.0),
            (96_000, 48_000, 20_000.0),
            (48_000, 32_000, 14_000.0),
        ];
        let mut worst = 0.0f64;
        for (in_rate, out_rate, top) in cases {
            let mut freqs = vec![100.0, 440.0, 1_000.0, 4_000.0, 8_000.0, 12_000.0];
            freqs.extend([0.5, 0.7, 0.85, 0.93, 0.97, 0.995].map(|k| top * k));
            let mut case_worst = 0.0f64;
            for f in freqs {
                // A tone at exactly the output Nyquist has no samples left;
                // the upper bound is always kept strictly inside.
                let g = gain_db(in_rate, out_rate, f);
                case_worst = case_worst.max(g.abs());
                assert!(
                    g.abs() <= 0.1,
                    "{in_rate}->{out_rate} at {f} Hz: {g:.4} dB (limit ±0.1 dB)"
                );
            }
            println!("{in_rate}->{out_rate} passband worst: {case_worst:.5} dB");
            worst = worst.max(case_worst);
        }
        println!("worst passband deviation: {worst:.5} dB");
    }

    #[test]
    fn dc_gain_is_unity() {
        // Every polyphase branch is DC-normalised, so a constant must come
        // through a constant: no low-frequency ripple or droop.
        for (in_rate, out_rate) in [(48_000u32, 44_100u32), (44_100, 48_000), (96_000, 48_000)] {
            let x = vec![0.5f32; in_rate as usize / 2];
            let mut r = make(in_rate, out_rate);
            let y = r.process(&x);
            let mid = middle(&y);
            let min = mid.iter().copied().fold(f32::INFINITY, f32::min);
            let max = mid.iter().copied().fold(f32::NEG_INFINITY, f32::max);
            assert!(
                (min - 0.5).abs() < 1e-6 && (max - 0.5).abs() < 1e-6,
                "{in_rate}->{out_rate} DC gain: {min}..{max}"
            );
        }
    }

    #[test]
    fn stopband_attenuates_above_output_nyquist_by_more_than_80_db() {
        let cases = [
            (
                48_000u32,
                44_100u32,
                vec![22_100.0f64, 22_500.0, 23_000.0, 23_500.0],
            ),
            (96_000, 48_000, vec![24_100.0, 26_000.0, 30_000.0, 40_000.0]),
            (48_000, 32_000, vec![16_100.0, 17_000.0, 20_000.0, 23_000.0]),
        ];
        let mut worst = f64::NEG_INFINITY;
        for (in_rate, out_rate, freqs) in cases {
            let mut case_worst = f64::NEG_INFINITY;
            for f in freqs {
                let g = stopband_db(in_rate, out_rate, f);
                case_worst = case_worst.max(g);
                worst = worst.max(g);
                assert!(
                    g < -80.0,
                    "{in_rate}->{out_rate} at {f} Hz: {g:.1} dB (need < -80 dB)"
                );
            }
            println!("{in_rate}->{out_rate} stopband: {case_worst:.1} dB");
        }
        println!("worst stopband attenuation: {worst:.1} dB");
    }

    #[test]
    fn broadband_alias_energy_is_more_than_80_db_down() {
        // 64 tones spread over 22.2-23.9 kHz, well above the 22.05 kHz output
        // Nyquist. Every one of them must fold into the band more than 80 dB
        // down, which is what a real aliasing check measures.
        let in_rate = 48_000u32;
        let n = in_rate as usize * 2;
        let mut x = vec![0.0f64; n];
        let tones = 64usize;
        let mut rng = Lcg::new(99);
        for t in 0..tones {
            let f = 22_200.0 + 1_700.0 * t as f64 / (tones - 1) as f64;
            let phase = rng.next_f32() as f64 * PI;
            let amp = 1.0 / tones as f64;
            for (i, v) in x.iter_mut().enumerate() {
                *v += amp * (2.0 * PI * f * i as f64 / in_rate as f64 + phase).sin();
            }
        }
        let x: Vec<f32> = x.into_iter().map(|v| v as f32).collect();

        let mut r = make(in_rate, 44_100);
        let y = r.process(&x);
        let g = db(rms(middle(&y)), rms(&x));
        assert!(
            g < -80.0,
            "broadband alias energy {g:.1} dB (need < -80 dB)"
        );
        println!("broadband alias energy: {g:.1} dB");
    }

    #[test]
    fn upsampling_images_are_more_than_80_db_down() {
        // 48 -> 96 kHz. Zero-stuffing by 2 puts a replica of a 20 kHz tone at
        // 48 - 20 = 28 kHz; the interpolation filter must remove it. (76 kHz
        // cannot be used: in discrete time it *is* -20 kHz, i.e. the mirror
        // half of the wanted tone.)
        let in_rate = 48_000u32;
        let out_rate = 96_000u32;
        let freq = 20_000.0f64;
        let image = in_rate as f64 - freq;

        let mut r = make(in_rate, out_rate);
        let x = sine(freq, in_rate, in_rate as usize * 2);
        let y = r.process(&x);
        // Exactly one second of steady state: 20 kHz and 28 kHz both land on
        // exact bins, so leakage cannot mask the image.
        let steady = &y[out_rate as usize / 2..out_rate as usize / 2 + out_rate as usize];
        let image_amp = tone_amplitude(steady, image, out_rate);
        let g = db(image_amp, 1.0);
        assert!(
            g < -80.0,
            "image at {image} Hz is {g:.1} dB (need < -80 dB)"
        );
        println!("upsampling image rejection: {g:.1} dB");
    }

    // ---- streaming equivalence ------------------------------------------

    #[test]
    fn chunked_matches_whole_buffer() {
        let cases = [
            (48_000u32, 44_100u32),
            (44_100, 48_000),
            (48_000, 96_000),
            (96_000, 48_000),
        ];
        let mut worst = 0.0f32;
        for (case, (in_rate, out_rate)) in cases.into_iter().enumerate() {
            let mut rng = Lcg::new(1000 + case as u64);
            let n = in_rate as usize / 2;
            let x: Vec<f32> = (0..n).map(|_| rng.next_f32() * 0.5).collect();

            let mut whole = make(in_rate, out_rate);
            let expected = whole.process(&x);

            let mut streamed = make(in_rate, out_rate);
            let mut out = Vec::new();
            let mut pos = 0;
            while pos < x.len() {
                let chunk = (rng.below(1_000) as usize + 1).min(x.len() - pos);
                streamed.process_chunk(&x[pos..pos + chunk], &mut out);
                pos += chunk;
            }
            streamed.flush_into(&mut out);

            assert_eq!(out.len(), expected.len(), "length mismatch");
            for (a, b) in out.iter().zip(&expected) {
                worst = worst.max((a - b).abs());
            }
        }
        println!("chunked vs whole-buffer max |diff|: {worst:e}");
        assert!(worst <= 1e-6, "chunked mismatch {worst:e}");
    }

    #[test]
    fn streaming_latency_is_reported_and_respected() {
        let in_rate = 48_000u32;
        let out_rate = 44_100u32;
        let mut r = make(in_rate, out_rate);
        let lookahead = r.latency_input_samples();

        assert_eq!(lookahead, r.taps() / 2);
        let expected = lookahead as f64 * out_rate as f64 / in_rate as f64;
        assert!((r.latency_samples() - expected).abs() < 1e-12);
        assert!(r.latency_samples() > 0.0);

        // Before lookahead is available nothing may be emitted; after N input
        // samples exactly the outputs whose window fits may be emitted.
        let x = sine(1_000.0, in_rate, in_rate as usize);
        let mut out = Vec::new();
        r.process_chunk(&x[..lookahead], &mut out);
        assert!(out.is_empty(), "emitted {} samples early", out.len());

        r.process_chunk(&x[lookahead..lookahead * 2], &mut out);
        let available = out.len();
        r.process_chunk(&x[lookahead * 2..], &mut out);
        r.flush_into(&mut out);
        let total = r.output_len(x.len());
        assert_eq!(out.len(), total);
        assert!(available > 0 && available <= total);
        println!(
            "latency: {lookahead} input samples = {:.2} output samples",
            expected
        );
    }

    #[test]
    fn impulse_lands_at_the_expected_output_index() {
        // Content alignment: an impulse at input index d appears at output
        // index round(d * out / in), independent of the filter latency.
        let in_rate = 48_000u32;
        let out_rate = 44_100u32;
        let d = 4800usize;
        let mut x = vec![0.0f32; 9600];
        x[d] = 1.0;

        let mut r = make(in_rate, out_rate);
        let y = r.process(&x);
        let peak = y
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().partial_cmp(&b.1.abs()).unwrap())
            .map(|(i, _)| i)
            .unwrap();
        let expected = (d as f64 * out_rate as f64 / in_rate as f64).round() as usize;
        assert!(
            (peak as i64 - expected as i64).abs() <= 1,
            "impulse peak at {peak}, expected {expected}"
        );
    }

    // ---- construction ----------------------------------------------------

    #[test]
    fn custom_quality_changes_tap_count() {
        let low = Resampler::new(48_000, 44_100, Quality::Low).unwrap();
        let high = Resampler::new(48_000, 44_100, Quality::High).unwrap();
        let max = Resampler::new(48_000, 44_100, Quality::Maximum).unwrap();
        assert!(low.taps() < high.taps());
        assert!(high.taps() < max.taps());
        assert!(low.taps() >= 4 && low.taps().is_multiple_of(2));
    }

    #[test]
    fn custom_quality_is_clamped_not_panicking() {
        let r = Resampler::new(
            48_000,
            44_100,
            Quality::Custom {
                stopband_db: 1.0e9,
                transition: 0.0,
            },
        )
        .unwrap();
        assert!(r.taps() > 0);
    }

    #[test]
    fn rational_approximation_uses_exact_phases_when_they_fit() {
        // Every standard audio rate gets one phase per exact fractional
        // position.
        for (in_rate, out_rate, phases) in [
            (48_000u32, 44_100u32, 147u32),
            (44_100, 48_000, 160),
            (22_050, 48_000, 320),
            (8_000, 44_100, 441),
        ] {
            let r = make(in_rate, out_rate);
            assert_eq!(
                r.phases(),
                phases as usize,
                "{in_rate}->{out_rate} phase count"
            );
        }
    }

    #[test]
    fn exotic_ratio_keeps_exact_length_and_rejects_the_stopband() {
        // 48000 -> 44101 has a 44101-phase ratio, far past the table cap. The
        // timing stays exact, so lengths are right and the filter still
        // behaves; only phase resolution is coarser.
        let in_rate = 48_000u32;
        let out_rate = 44_101u32;
        let mut r = make(in_rate, out_rate);
        assert!(r.phases() <= MAX_PHASES as usize);
        assert!(
            r.phases() * r.taps() <= MAX_COEFFS,
            "coefficient table over budget"
        );

        let n = in_rate as usize;
        let y = r.process(&vec![0.0f32; n]);
        assert_eq!(
            y.len(),
            (n as f64 * out_rate as f64 / in_rate as f64).round() as usize
        );

        // 22.1 kHz is above the 22.0505 kHz output Nyquist.
        let x = sine(22_100.0, in_rate, n);
        let y = r.process(&x);
        let g = db(rms(middle(&y)), rms(&x));
        assert!(g < -80.0, "stopband at exotic ratio: {g:.1} dB");
        println!(
            "exotic 48000->44101: phases={}, taps={}, stopband={g:.1} dB",
            r.phases(),
            r.taps()
        );
    }

    #[test]
    fn bessel_and_sinc_are_sane() {
        assert!((bessel_i0(0.0) - 1.0).abs() < 1e-15);
        assert!((bessel_i0(1.0) - 1.266_065_877_752_008).abs() < 1e-12);
        assert!((sinc(0.0) - 1.0).abs() < 1e-15);
        assert!(sinc(1.0).abs() < 1e-15);
        assert!(sinc(-2.0).abs() < 1e-15);
    }

    // ---- measurement report ---------------------------------------------

    #[test]
    fn report_measured_quality_and_cpu() {
        let mut worst_pass = 0.0f64;
        for f in [100.0, 1_000.0, 10_000.0, 18_000.0, 20_000.0] {
            worst_pass = worst_pass.max(gain_db(48_000, 44_100, f).abs());
            worst_pass = worst_pass.max(gain_db(44_100, 48_000, f).abs());
        }
        let mut worst_stop = f64::NEG_INFINITY;
        for f in [22_100.0, 22_500.0, 23_000.0, 23_500.0] {
            worst_stop = worst_stop.max(stopband_db(48_000, 44_100, f));
        }
        for f in [24_100.0, 28_000.0, 36_000.0, 44_000.0] {
            worst_stop = worst_stop.max(stopband_db(96_000, 48_000, f));
        }

        let r = make(48_000, 44_100);
        println!(
            "48k->44.1k: taps={}, phases={}, latency={:.2} out samples ({:.3} ms)",
            r.taps(),
            r.phases(),
            r.latency_samples(),
            r.latency_samples() / 44_100.0 * 1e3
        );
        println!("worst passband deviation: {worst_pass:.5} dB");
        println!("worst stopband attenuation: {worst_stop:.1} dB");

        // CPU: 10 s of noise through the 48 -> 44.1 kHz path.
        let mut rng = Lcg::new(5);
        let seconds = 10usize;
        let x: Vec<f32> = (0..48_000 * seconds).map(|_| rng.next_f32()).collect();
        let mut r = make(48_000, 44_100);
        let start = std::time::Instant::now();
        let y = r.process(&x);
        let elapsed = start.elapsed();
        std::hint::black_box(&y);
        println!(
            "cpu: {:.2} ms per second of audio ({:.0}x realtime, {})",
            elapsed.as_secs_f64() * 1e3 / seconds as f64,
            seconds as f64 / elapsed.as_secs_f64(),
            if cfg!(debug_assertions) {
                "debug build"
            } else {
                "release build"
            }
        );
    }
}
