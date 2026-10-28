//! Short-time Fourier transform (STFT) analysis and synthesis.
//!
//! This is the shared front end for the phase vocoder (PLAN P-02), the
//! envelope/formant stage (P-03) and the channel vocoder (B-02): framing, a
//! periodic Hann window, an in-house radix-2 real FFT, complex one-sided
//! spectra, and overlap-add synthesis that is an exact inverse of analysis
//! when the bins are left untouched.
//!
//! # Framing, delay and edge handling
//!
//! Let `N` be [`StftConfig::window_size`] and `H` the hop. Analysis frame `t`
//! is the windowed input segment starting at padded index `t·H`, where the
//! input is treated as if it were padded with `N` zeros in front. In *input*
//! coordinates that frame is centred at `t·H − N/2`, so consecutive frame
//! centres are exactly `H` apart. Enough zeros are appended at the end to
//! complete the last frame:
//!
//! ```text
//! padded:   [ N zeros ][ input ][ zeros ]
//! frames:   |--- t=0 ---|
//!               |--- t=1 ---|
//!                   |--- t=2 ---|
//! ```
//!
//! The window is applied twice (once in analysis, once in synthesis), so the
//! overlap-add is normalised by the sum of the **squared** window across the
//! four overlapping frames. For a periodic Hann at `H = N/4` that sum is
//! exactly `1.5` and flat to better than `1e-6` (asserted by the tests), which
//! is what makes the round trip an identity.
//!
//! * [`Stft::analyze`] + [`Stft::synthesize`] return exactly `input.len()`
//!   samples, sample-aligned: output sample `i` is input sample `i`. The
//!   leading `N` padded samples and the trailing pad never reach the output.
//! * No frames are "dropped" and no input samples are lost: edge frames are
//!   analysed against the zero pad, and only the ramp-up/ramp-down regions
//!   (which correspond to padded zeros) are discarded.
//! * [`StftStream::latency_samples`] is exactly `N` samples. Output sample `i`
//!   needs the frame that starts at or before `i`, which ends at `i + N`, so
//!   nothing can be emitted until `N` input samples have arrived. That is
//!   21.3 ms at `N = 1024`, 42.7 ms at `2048` and 85.3 ms at `4096`, at
//!   48 kHz.
//! * After the first `N` samples, streaming output keeps pace with input;
//!   [`StftStream::flush_into`] drains the tail. `process_chunk` output is
//!   bit-identical to the whole-buffer path for any chunk sizes.
//!
//! # Window and hop
//!
//! The window is a **periodic** Hann, `w[n] = 0.5 − 0.5·cos(2πn/N)`, so
//! `w[0] = 0` and `w[N/2] = 1`. It is not the symmetric variant: the periodic
//! form is the one whose squared overlap-add is flat at `H = N/4`.
//!
//! `H = N/4` is the only accepted hop. It is a validated configuration, not a
//! silent default: the squared-Hann COLA constant and the phase-vocoder
//! unambiguous deviation bound (`sr/(2H)`, see `docs/corpus.md` §5.1) are both
//! derived for 75% overlap. A different hop cannot promise identity
//! reconstruction with this window pairing, so [`Stft::new`] rejects it with
//! [`StftError::UnsupportedHop`] rather than returning subtly wrong audio.
//!
//! Any power-of-two `N` from [`MIN_WINDOW_SIZE`] to [`MAX_WINDOW_SIZE`] is
//! accepted (the radix-2 FFT falls out naturally); the project uses
//! `N ∈ {1024, 2048, 4096}`. Non-power-of-two sizes are rejected, not
//! rounded.
//!
//! # Choosing `N`: measured guidance (F-06)
//!
//! The corpus (`docs/corpus.md`) has voice median F0 from **79 Hz to 323 Hz**,
//! with a lowest clean p10 of **71 Hz**. At 48 kHz:
//!
//! * `N = 2048` has 23.44 Hz bins, so 71 Hz is **3.0 bins** — the usual
//!   three-bins-per-period rule is just met. **2048 is safe unshifted for the
//!   whole corpus.**
//! * At **−12 st** a low male voice (the `am_onyx` low anchor, 88 Hz) lands at
//!   **44 Hz**, and the p10 of the same voice at 71 Hz lands at 36 Hz. At
//!   `N = 2048` that is **≈1.5–1.9 bins**: there is no usable estimate of the
//!   fundamental. **Use `N = 4096` whenever a preset can reach −12 st on a
//!   low male voice** (36 Hz becomes 3.0 bins), as D2 and P-01 require.
//! * `N = 1024` has 46.9 Hz bins and a three-bin floor of 141 Hz. It cannot
//!   resolve most of the corpus and has no safe use here; it exists for
//!   latency experiments only.
//!
//! If a caller ignores this and runs a −12 st shift on a low male at
//! `N = 2048`, the partials of the shifted voice fall inside one or two bins.
//! The phase vocoder then cannot separate the fundamental from its neighbours:
//! expect octave errors (the tracker locks to a harmonic), a hollow or buzzy
//! output where the fundamental drops out, and rough, unstable pitch. Nothing
//! in this module prevents that — the analysis is correct, the resolution is
//! simply not there. `N` must be chosen per preset/render call, not per voice,
//! so A/B and golden renders stay comparable (`docs/corpus.md` §5.1).
//!
//! # Allocation
//!
//! [`Stft::analyze_frame`] and [`Stft::synthesize_frame`] allocate nothing:
//! they write into caller-provided scratch slices. [`Stft::analyze_into`]
//! reuses its spectrogram and internal frame scratch, and
//! [`Stft::synthesize_into`] reuses its overlap-add accumulator and output
//! vector after the first call; the per-frame loops themselves never
//! allocate. [`StftStream`] preallocates bounded input-history and
//! overlap-add buffers, so steady-state hop-sized chunks allocate nothing.
//!
//! No `unsafe`, no threads, no clocks, no dependencies.

use std::f64::consts::PI as PI_F64;
use std::fmt;

/// Smallest accepted window size, in samples.
pub const MIN_WINDOW_SIZE: usize = 64;
/// Largest accepted window size, in samples.
pub const MAX_WINDOW_SIZE: usize = 1 << 16;

/// Complex number with `f32` components, used for one-sided spectral bins.
///
/// Deliberately minimal: this crate is dependency-free (no `num-complex`) and
/// only needs the operations a phase vocoder performs on a bin.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Complex {
    /// Real part.
    pub re: f32,
    /// Imaginary part.
    pub im: f32,
}

impl Complex {
    /// The zero bin.
    pub const ZERO: Complex = Complex { re: 0.0, im: 0.0 };

    /// Construct from rectangular components.
    pub const fn new(re: f32, im: f32) -> Self {
        Complex { re, im }
    }

    /// Construct from magnitude and phase (radians).
    pub fn from_polar(magnitude: f32, phase: f32) -> Self {
        Complex {
            re: magnitude * phase.cos(),
            im: magnitude * phase.sin(),
        }
    }

    /// `sqrt(re² + im²)`.
    pub fn magnitude(self) -> f32 {
        self.re.hypot(self.im)
    }

    /// `re² + im²`, without the square root.
    pub fn magnitude_sq(self) -> f32 {
        self.re * self.re + self.im * self.im
    }

    /// Phase in radians, in `(-π, π]`.
    pub fn phase(self) -> f32 {
        self.im.atan2(self.re)
    }

    /// Complex conjugate.
    pub fn conj(self) -> Self {
        Complex {
            re: self.re,
            im: -self.im,
        }
    }

    /// Multiply both components by a real scalar.
    pub fn scale(self, factor: f32) -> Self {
        Complex {
            re: self.re * factor,
            im: self.im * factor,
        }
    }
}

impl core::ops::Add for Complex {
    type Output = Complex;

    fn add(self, rhs: Complex) -> Complex {
        Complex {
            re: self.re + rhs.re,
            im: self.im + rhs.im,
        }
    }
}

impl core::ops::AddAssign for Complex {
    fn add_assign(&mut self, rhs: Complex) {
        self.re += rhs.re;
        self.im += rhs.im;
    }
}

impl core::ops::Sub for Complex {
    type Output = Complex;

    fn sub(self, rhs: Complex) -> Complex {
        Complex {
            re: self.re - rhs.re,
            im: self.im - rhs.im,
        }
    }
}

impl core::ops::SubAssign for Complex {
    fn sub_assign(&mut self, rhs: Complex) {
        self.re -= rhs.re;
        self.im -= rhs.im;
    }
}

impl core::ops::Mul for Complex {
    type Output = Complex;

    fn mul(self, rhs: Complex) -> Complex {
        Complex {
            re: self.re * rhs.re - self.im * rhs.im,
            im: self.re * rhs.im + self.im * rhs.re,
        }
    }
}

impl core::ops::Mul<f32> for Complex {
    type Output = Complex;

    fn mul(self, rhs: f32) -> Complex {
        self.scale(rhs)
    }
}

impl core::ops::MulAssign for Complex {
    fn mul_assign(&mut self, rhs: Complex) {
        *self = *self * rhs;
    }
}

impl core::ops::Neg for Complex {
    type Output = Complex;

    fn neg(self) -> Complex {
        Complex {
            re: -self.re,
            im: -self.im,
        }
    }
}

/// Construction and framing errors. Returned instead of panicking, so a bad
/// configuration from a caller (or from WASM params) is a typed failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StftError {
    /// `window_size` is zero, not a power of two, or outside
    /// `MIN_WINDOW_SIZE..=MAX_WINDOW_SIZE`.
    UnsupportedWindowSize {
        /// The rejected size.
        window_size: usize,
    },
    /// `hop` is not `window_size / 4`.
    UnsupportedHop {
        /// The requested hop.
        hop: usize,
        /// The window it was paired with.
        window_size: usize,
        /// The only hop this transform accepts.
        expected: usize,
    },
    /// A time-domain frame had the wrong length.
    FrameLength {
        /// Required length (`window_size`).
        expected: usize,
        /// Supplied length.
        actual: usize,
    },
    /// A one-sided spectrum had the wrong length.
    BinLength {
        /// Required length (`window_size / 2 + 1`).
        expected: usize,
        /// Supplied length.
        actual: usize,
    },
    /// A [`Spectrogram`] did not match the transform, or its frame count did
    /// not match the requested output length.
    Geometry {
        /// Which field disagreed (`"frames"`, `"bins"`, `"hop"`,
        /// `"window_size"`).
        field: &'static str,
        /// Value the transform required.
        expected: usize,
        /// Value the spectrogram had.
        actual: usize,
    },
}

impl fmt::Display for StftError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StftError::UnsupportedWindowSize { window_size } => write!(
                f,
                "window size {window_size} is not supported: use a power of two in \
                 {MIN_WINDOW_SIZE}..={MAX_WINDOW_SIZE}"
            ),
            StftError::UnsupportedHop {
                hop,
                window_size,
                expected,
            } => write!(
                f,
                "hop {hop} is not supported for window {window_size}: use {expected} \
                 (window/4); Hann squared-COLA and the phase-deviation bound assume \
                 75% overlap"
            ),
            StftError::FrameLength { expected, actual } => {
                write!(f, "frame length {actual}, expected {expected}")
            }
            StftError::BinLength { expected, actual } => {
                write!(f, "spectrum length {actual}, expected {expected}")
            }
            StftError::Geometry {
                field,
                expected,
                actual,
            } => write!(f, "spectrogram {field} is {actual}, expected {expected}"),
        }
    }
}

impl std::error::Error for StftError {}

/// Window and hop for an STFT.
///
/// The default is PLAN D2: `N = 2048`, Hann, 75% overlap (`hop = 512`) at
/// 48 kHz. See the [module docs](self) for when to choose `4096`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StftConfig {
    /// Window (and FFT) size in samples; a power of two.
    pub window_size: usize,
    /// Hop between window origins; must be `window_size / 4`.
    pub hop: usize,
}

impl StftConfig {
    /// Build a configuration. Validation happens in [`Stft::new`].
    pub const fn new(window_size: usize, hop: usize) -> Self {
        StftConfig { window_size, hop }
    }

    /// The D2 default: 2048-sample Hann, hop 512.
    pub const fn d2() -> Self {
        StftConfig {
            window_size: 2048,
            hop: 512,
        }
    }
}

impl Default for StftConfig {
    fn default() -> Self {
        StftConfig::d2()
    }
}

/// A one-sided real spectrum.
///
/// `bins = window_size / 2 + 1` complex values for every analysed frame:
/// bin `0` is DC, bin `bins - 1` is Nyquist, and both have zero imaginary
/// part. Conjugate symmetry for the negative frequencies is implicit.
///
/// Frame `t` occupies `data[t * bins .. (t + 1) * bins]`. Use
/// [`frame`](Spectrogram::frame) / [`frame_mut`](Spectrogram::frame_mut) or
/// the iterators instead of doing that arithmetic at call sites.
#[derive(Clone, Debug, PartialEq)]
pub struct Spectrogram {
    frames: usize,
    bins: usize,
    hop: usize,
    window_size: usize,
    data: Vec<Complex>,
}

impl Spectrogram {
    /// Allocate a zeroed spectrogram with the given geometry.
    pub fn new(frames: usize, bins: usize, hop: usize, window_size: usize) -> Self {
        Spectrogram {
            frames,
            bins,
            hop,
            window_size,
            data: vec![Complex::ZERO; frames.saturating_mul(bins)],
        }
    }

    /// Number of frames.
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Bins per frame (`window_size / 2 + 1`).
    pub fn bins(&self) -> usize {
        self.bins
    }

    /// Hop the spectrogram was analysed with.
    pub fn hop(&self) -> usize {
        self.hop
    }

    /// Window size the spectrogram was analysed with.
    pub fn window_size(&self) -> usize {
        self.window_size
    }

    /// Total number of complex bins (`frames * bins`).
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Whether there are no frames.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// All bins, frame-major.
    pub fn data(&self) -> &[Complex] {
        &self.data
    }

    /// All bins, frame-major, for in-place modification.
    pub fn data_mut(&mut self) -> &mut [Complex] {
        &mut self.data
    }

    /// One frame's bins, or `None` when `frame >= frames()`.
    pub fn frame(&self, frame: usize) -> Option<&[Complex]> {
        if frame >= self.frames {
            return None;
        }
        Some(&self.data[frame * self.bins..(frame + 1) * self.bins])
    }

    /// One frame's bins, mutable, or `None` when `frame >= frames()`.
    pub fn frame_mut(&mut self, frame: usize) -> Option<&mut [Complex]> {
        if frame >= self.frames {
            return None;
        }
        Some(&mut self.data[frame * self.bins..(frame + 1) * self.bins])
    }

    /// Iterate frames in order.
    pub fn frames_iter(&self) -> impl Iterator<Item = &[Complex]> {
        self.data.chunks_exact(self.bins)
    }

    /// Iterate frames in order, mutably.
    pub fn frames_iter_mut(&mut self) -> impl Iterator<Item = &mut [Complex]> {
        let bins = self.bins;
        self.data.chunks_exact_mut(bins)
    }

    /// Resize and zero-fill. Used internally by `analyze_into`; kept public so
    /// a render loop can reuse one allocation.
    pub fn reset(&mut self, frames: usize, bins: usize, hop: usize, window_size: usize) {
        self.frames = frames;
        self.bins = bins;
        self.hop = hop;
        self.window_size = window_size;
        self.data.clear();
        self.data.resize(frames.saturating_mul(bins), Complex::ZERO);
    }
}

// ---------------------------------------------------------------------------
// Radix-2 real FFT
// ---------------------------------------------------------------------------

/// In-place complex FFT of `radix-2` length `data.len()`.
///
/// Unnormalised: forward is `Σ x[n]·e^(−2πikn/N)`, inverse is
/// `Σ X[k]·e^(+2πikn/N)` (the caller scales). `bitrev` and `twiddle` are the
/// precomputed tables for this length.
fn cfft_forward(data: &mut [Complex], bitrev: &[u32], twiddle: &[Complex]) {
    let n = data.len();
    debug_assert!(n.is_power_of_two());
    debug_assert_eq!(bitrev.len(), n);

    for (i, &j) in bitrev.iter().enumerate() {
        let j = j as usize;
        if i < j {
            data.swap(i, j);
        }
    }

    let mut len = 2;
    while len <= n {
        let half = len / 2;
        let step = n / len;
        for start in (0..n).step_by(len) {
            for k in 0..half {
                let w = twiddle[k * step];
                let u = data[start + k];
                let v = data[start + k + half] * w;
                data[start + k] = u + v;
                data[start + k + half] = u - v;
            }
        }
        len <<= 1;
    }
}

/// Normalised in-place inverse complex FFT.
fn cfft_inverse(data: &mut [Complex], bitrev: &[u32], twiddle: &[Complex]) {
    for value in data.iter_mut() {
        *value = value.conj();
    }
    cfft_forward(data, bitrev, twiddle);
    let scale = 1.0 / data.len() as f32;
    for value in data.iter_mut() {
        *value = value.conj().scale(scale);
    }
}

/// Radix-2 real FFT of a single even length `N`.
///
/// Forward packs the real signal into `N/2` complex values and post-processes
/// the half-size transform; inverse undoes that and unpacks. Both directions
/// are `O(N log N)` and allocate nothing after construction (`buf` is reused).
#[derive(Debug)]
struct Fft {
    /// Real transform length `N`.
    size: usize,
    /// Half length `M = N/2`; also the complex transform length.
    half: usize,
    /// Bit-reversal permutation of length `M`.
    bitrev: Vec<u32>,
    /// `e^(−2πik/N)` for `k in 0..M` (used by the packing step).
    twiddle_n: Vec<Complex>,
    /// `e^(−2πik/M)` for `k in 0..M/2` (used by the complex FFT).
    twiddle_m: Vec<Complex>,
    /// Reusable complex scratch of length `M`.
    buf: Vec<Complex>,
}

impl Fft {
    fn new(size: usize) -> Self {
        debug_assert!(size.is_power_of_two());
        debug_assert!(size >= 4);
        let half = size / 2;

        let bits = half.trailing_zeros();
        let mut bitrev = Vec::with_capacity(half);
        for i in 0..half {
            let mut reversed = 0usize;
            for bit in 0..bits {
                reversed = (reversed << 1) | ((i >> bit) & 1);
            }
            bitrev.push(reversed as u32);
        }

        let mut twiddle_n = Vec::with_capacity(half);
        for k in 0..half {
            let angle = -2.0 * PI_F64 * k as f64 / size as f64;
            twiddle_n.push(Complex::new(angle.cos() as f32, angle.sin() as f32));
        }

        let mut twiddle_m = Vec::with_capacity(half / 2);
        for k in 0..half / 2 {
            let angle = -2.0 * PI_F64 * k as f64 / half as f64;
            twiddle_m.push(Complex::new(angle.cos() as f32, angle.sin() as f32));
        }

        Fft {
            size,
            half,
            bitrev,
            twiddle_n,
            twiddle_m,
            buf: vec![Complex::ZERO; half],
        }
    }

    /// Real forward transform: `out[k] = Σ x[n]·e^(−2πikn/N)` for
    /// `k = 0..=N/2`. Unnormalised.
    ///
    /// `x.len()` must be `N` and `out.len()` must be `N/2 + 1`; both are
    /// guaranteed by the module's validated callers.
    fn forward(&mut self, x: &[f32], out: &mut [Complex]) {
        let m = self.half;
        debug_assert_eq!(x.len(), self.size);
        debug_assert_eq!(out.len(), m + 1);

        for (slot, pair) in self.buf.iter_mut().zip(x.as_chunks::<2>().0) {
            *slot = Complex::new(pair[0], pair[1]);
        }
        cfft_forward(&mut self.buf, &self.bitrev, &self.twiddle_m);

        let z0 = self.buf[0];
        out[0] = Complex::new(z0.re + z0.im, 0.0);
        out[m] = Complex::new(z0.re - z0.im, 0.0);

        // For k in 1..=M/2 the pair (k, M-k) is recovered from the same two
        // half-size bins; X[M-k] = conj(E[k] - W_N^k·O[k]).
        for k in 1..=m / 2 {
            let a = self.buf[k];
            let b = self.buf[m - k].conj();
            let even = (a + b).scale(0.5);
            // O[k] = (a - b) / (2i) = −i·(a − b)/2: scale, then rotate by +i.
            let rotated = (a - b).scale(-0.5);
            let odd = Complex::new(-rotated.im, rotated.re);
            let w = self.twiddle_n[k];
            let wk_odd = w * odd;
            out[k] = even + wk_odd;
            if k != m - k {
                out[m - k] = (even - wk_odd).conj();
            }
        }
    }

    /// Real inverse transform, normalised: reconstructs `x` from `bins`.
    ///
    /// `out.len()` must be `N` and `bins.len()` must be `N/2 + 1`.
    fn inverse(&mut self, bins: &[Complex], out: &mut [f32]) {
        let m = self.half;
        debug_assert_eq!(out.len(), self.size);
        debug_assert_eq!(bins.len(), m + 1);

        // X[k] = E[k] + W_N^k·O[k] and X[M-k] = conj(E[k] − W_N^k·O[k]).
        for k in 0..m {
            let xk = bins[k];
            let xmk = if k == 0 { bins[m] } else { bins[m - k] };
            let even = (xk + xmk.conj()).scale(0.5);
            let diff = (xk - xmk.conj()).scale(0.5);
            let odd = diff * self.twiddle_n[k].conj();
            self.buf[k] = even + Complex::new(-odd.im, odd.re);
        }

        cfft_inverse(&mut self.buf, &self.bitrev, &self.twiddle_m);

        for (pair, z) in out.as_chunks_mut::<2>().0.iter_mut().zip(self.buf.iter()) {
            pair[0] = z.re;
            pair[1] = z.im;
        }
    }
}

/// Real FFT of one fixed even length, exposed inside the crate for the
/// cepstral analysis in [`crate::envelope`] (P-03).
///
/// This is the same engine the STFT uses: an unnormalised forward transform
/// producing `N/2 + 1` one-sided bins and a normalised inverse transform that
/// takes `N/2 + 1` Hermitian-symmetric bins back to `N` real samples. It is
/// `pub(crate)` so the envelope module can reuse the radix-2 plan without
/// duplicating an FFT, and so the crate's public surface stays unchanged.
pub(crate) struct RealFft(Fft);

impl RealFft {
    /// Build a plan for a power-of-two length `n >= 4`.
    pub(crate) fn new(n: usize) -> Self {
        RealFft(Fft::new(n))
    }

    /// `out[k] = Σ x[n]·e^(−2πikn/N)` for `k = 0..=N/2`, unnormalised.
    pub(crate) fn forward(&mut self, x: &[f32], out: &mut [Complex]) {
        self.0.forward(x, out);
    }

    /// Normalised inverse of a one-sided, Hermitian-symmetric spectrum.
    pub(crate) fn inverse(&mut self, bins: &[Complex], out: &mut [f32]) {
        self.0.inverse(bins, out);
    }
}

// ---------------------------------------------------------------------------
// STFT
// ---------------------------------------------------------------------------

/// A validated STFT configuration plus its window, FFT plans and scratch.
///
/// Use [`analyze_frame`](Stft::analyze_frame) /
/// [`synthesize_frame`](Stft::synthesize_frame) for allocation-free per-frame
/// work, [`analyze`](Stft::analyze) / [`synthesize`](Stft::synthesize) for
/// whole buffers, or [`StftStream`] for hop-sized chunks.
pub struct Stft {
    config: StftConfig,
    window: Vec<f32>,
    fft: Fft,
    /// Windowed frame, `N` samples. Reused by `analyze_frame`.
    scratch: Vec<f32>,
    /// Reusable overlap-add accumulator for `synthesize_into`.
    acc: Vec<f32>,
    /// Reusable synthesized-frame scratch for `synthesize_into`.
    time: Vec<f32>,
    /// `Σ_m w²(n + mH)`, the flat overlap-add gain (1.5 for Hann at N/4).
    cola_sum: f32,
    /// `1 / cola_sum`, applied once during overlap-add.
    inv_cola: f32,
}

impl fmt::Debug for Stft {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Stft")
            .field("config", &self.config)
            .field("bins", &self.bins())
            .field("cola_sum", &self.cola_sum)
            .finish_non_exhaustive()
    }
}

impl Stft {
    /// Validate a configuration and build the window and FFT plans.
    ///
    /// Returns [`StftError::UnsupportedWindowSize`] for a size that is not a
    /// power of two in `MIN_WINDOW_SIZE..=MAX_WINDOW_SIZE` and
    /// [`StftError::UnsupportedHop`] for any hop other than `window_size / 4`.
    pub fn new(config: StftConfig) -> Result<Self, StftError> {
        let n = config.window_size;
        if !n.is_power_of_two() || !(MIN_WINDOW_SIZE..=MAX_WINDOW_SIZE).contains(&n) {
            return Err(StftError::UnsupportedWindowSize { window_size: n });
        }
        let expected = n / 4;
        if config.hop != expected {
            return Err(StftError::UnsupportedHop {
                hop: config.hop,
                window_size: n,
                expected,
            });
        }

        let window = periodic_hann(n);

        // Σ_m w²(n + mH) is independent of n for this window/hop; evaluate it
        // at n = 0 in f64 so the single normalisation constant is as exact as
        // the stored f32 window allows.
        let mut cola = 0.0f64;
        for m in 0..4 {
            let w = window[(m * expected) % n] as f64;
            cola += w * w;
        }
        let cola_sum = cola as f32;

        Ok(Stft {
            config,
            window,
            fft: Fft::new(n),
            scratch: vec![0.0; n],
            acc: Vec::new(),
            time: vec![0.0; n],
            cola_sum,
            inv_cola: 1.0 / cola_sum,
        })
    }

    /// The configuration this transform was built with.
    pub fn config(&self) -> StftConfig {
        self.config
    }

    /// Window size `N`, in samples.
    pub fn window_size(&self) -> usize {
        self.config.window_size
    }

    /// Hop `H = N/4`, in samples.
    pub fn hop(&self) -> usize {
        self.config.hop
    }

    /// Number of one-sided bins per frame: `N/2 + 1`.
    pub fn bins(&self) -> usize {
        self.config.window_size / 2 + 1
    }

    /// Centre of analysis frame `frame` in input-sample coordinates.
    ///
    /// This is `frame·H − N/2`, so it is negative for the first frames (they
    /// sit in the leading zero pad) and frame `N/(2H)` is centred on input
    /// sample 0. Consecutive centres are exactly `H` apart.
    pub fn frame_center(&self, frame: usize) -> i64 {
        frame as i64 * self.config.hop as i64 - self.config.window_size as i64 / 2
    }

    /// The periodic Hann window (length `N`).
    pub fn window(&self) -> &[f32] {
        &self.window
    }

    /// Streaming lookahead: exactly `N` samples.
    ///
    /// Output sample `i` depends on the frame starting at or before `i`, which
    /// ends at `i + N`, so the streaming path cannot emit anything until `N`
    /// input samples are buffered. Whole-buffer output is sample-aligned and
    /// has no offset.
    pub fn latency_samples(&self) -> usize {
        self.config.window_size
    }

    /// [`latency_samples`](Stft::latency_samples) in seconds, or `0.0` when
    /// `sample_rate` is zero.
    pub fn latency_seconds(&self, sample_rate: u32) -> f32 {
        if sample_rate == 0 {
            0.0
        } else {
            self.latency_samples() as f32 / sample_rate as f32
        }
    }

    /// Width of one bin in Hz, or `0.0` when `sample_rate` is zero.
    pub fn bin_width_hz(&self, sample_rate: u32) -> f32 {
        if sample_rate == 0 {
            0.0
        } else {
            sample_rate as f32 / self.config.window_size as f32
        }
    }

    /// Centre frequency of `bin` in Hz, clamped to Nyquist, or `0.0` when
    /// `sample_rate` is zero.
    pub fn bin_hz(&self, bin: usize, sample_rate: u32) -> f32 {
        let bin = bin.min(self.bins() - 1);
        bin as f32 * self.bin_width_hz(sample_rate)
    }

    /// Number of frames [`analyze`](Stft::analyze) produces for
    /// `input_len` samples.
    ///
    /// Zero for an empty input. Otherwise `ceil((N + input_len) / H)`: the
    /// frames needed to cover the `N`-sample pad plus the input.
    pub fn frame_count(&self, input_len: usize) -> usize {
        if input_len == 0 {
            return 0;
        }
        self.config
            .window_size
            .saturating_add(input_len)
            .div_ceil(self.config.hop)
    }

    /// Window and transform one frame.
    ///
    /// `frame.len()` must be `N` and `out.len()` must be [`bins`](Stft::bins);
    /// otherwise a typed error is returned. Allocation-free.
    pub fn analyze_frame(&mut self, frame: &[f32], out: &mut [Complex]) -> Result<(), StftError> {
        let n = self.config.window_size;
        if frame.len() != n {
            return Err(StftError::FrameLength {
                expected: n,
                actual: frame.len(),
            });
        }
        if out.len() != self.bins() {
            return Err(StftError::BinLength {
                expected: self.bins(),
                actual: out.len(),
            });
        }
        self.analyze_windowed(frame, out);
        Ok(())
    }

    /// Inverse-transform one frame and apply the synthesis window.
    ///
    /// `bins.len()` must be `N/2 + 1` and `out.len()` must be `N`. The
    /// overlap-add gain is **not** applied here; the whole-buffer and
    /// streaming paths apply it once when they sum frames. Allocation-free.
    pub fn synthesize_frame(&mut self, bins: &[Complex], out: &mut [f32]) -> Result<(), StftError> {
        if bins.len() != self.bins() {
            return Err(StftError::BinLength {
                expected: self.bins(),
                actual: bins.len(),
            });
        }
        let n = self.config.window_size;
        if out.len() != n {
            return Err(StftError::FrameLength {
                expected: n,
                actual: out.len(),
            });
        }
        self.synthesize_weighted(bins, out);
        Ok(())
    }

    /// Analyse a whole buffer into a new spectrogram.
    ///
    /// The returned frames use the padded-origin convention described in the
    /// [module docs](self). [`synthesize`](Stft::synthesize) turns an
    /// unmodified result back into the input.
    pub fn analyze(&mut self, input: &[f32]) -> Spectrogram {
        let mut spectrogram = Spectrogram::new(
            self.frame_count(input.len()),
            self.bins(),
            self.config.hop,
            self.config.window_size,
        );
        self.analyze_into(input, &mut spectrogram);
        spectrogram
    }

    /// Analyse a whole buffer into an existing spectrogram, reusing its
    /// allocation. Resets the geometry to match `input`.
    pub fn analyze_into(&mut self, input: &[f32], spectrogram: &mut Spectrogram) {
        let frames = self.frame_count(input.len());
        spectrogram.reset(
            frames,
            self.bins(),
            self.config.hop,
            self.config.window_size,
        );

        let n = self.config.window_size;
        let hop = self.config.hop;
        for t in 0..frames {
            let origin = t * hop;
            // Assemble the zero-padded frame directly in the windowed scratch
            // buffer: padded index origin+i maps to input index origin+i-N.
            for (i, slot) in self.scratch.iter_mut().enumerate() {
                let input_index = origin as i64 + i as i64 - n as i64;
                *slot = if input_index >= 0 {
                    input.get(input_index as usize).copied().unwrap_or(0.0)
                } else {
                    0.0
                };
            }
            for (value, &w) in self.scratch.iter_mut().zip(&self.window) {
                *value *= w;
            }
            let bins = spectrogram
                .frame_mut(t)
                .expect("frame count and geometry were just set");
            self.fft.forward(&self.scratch, bins);
        }
    }

    /// Overlap-add a spectrogram back to `out_len` samples.
    ///
    /// `out_len` must match the input length used for
    /// [`analyze`](Stft::analyze): the required frame count is derived from it
    /// and checked. Output sample `i` corresponds to input sample `i`.
    pub fn synthesize(
        &mut self,
        spectrogram: &Spectrogram,
        out_len: usize,
    ) -> Result<Vec<f32>, StftError> {
        let mut out = Vec::with_capacity(out_len);
        self.synthesize_into(spectrogram, out_len, &mut out)?;
        Ok(out)
    }

    /// Overlap-add into an existing vector, clearing it first and reusing its
    /// allocation.
    pub fn synthesize_into(
        &mut self,
        spectrogram: &Spectrogram,
        out_len: usize,
        out: &mut Vec<f32>,
    ) -> Result<(), StftError> {
        self.check_geometry(spectrogram, out_len)?;

        let n = self.config.window_size;
        let hop = self.config.hop;
        let bins = self.bins();
        let total = n.saturating_add(out_len);
        let frames = spectrogram.frames();
        self.acc.clear();
        self.acc.resize(total, 0.0);
        // Move the scratch out so `synthesize_weighted` can borrow `self`.
        let mut time = core::mem::take(&mut self.time);

        for t in 0..frames {
            let origin = t * hop;
            if origin >= total {
                break;
            }
            let src = &spectrogram.data[t * bins..(t + 1) * bins];
            self.synthesize_weighted(src, &mut time);
            let count = (total - origin).min(n);
            for (dst, &value) in self.acc[origin..origin + count]
                .iter_mut()
                .zip(&time[..count])
            {
                *dst += value;
            }
        }
        self.time = time;

        out.clear();
        out.reserve(out_len);
        let gain = self.inv_cola;
        out.extend(self.acc[n..n + out_len].iter().map(|&value| value * gain));
        Ok(())
    }

    /// Analysis followed by synthesis with no spectral modification.
    ///
    /// Returns exactly `input.len()` samples, sample-aligned with the input.
    /// This is the whole-buffer identity path; [`StftStream`] produces the
    /// same bytes from arbitrary chunks.
    pub fn roundtrip(&mut self, input: &[f32]) -> Result<Vec<f32>, StftError> {
        let spectrogram = self.analyze(input);
        self.synthesize(&spectrogram, input.len())
    }

    fn check_geometry(&self, spectrogram: &Spectrogram, out_len: usize) -> Result<(), StftError> {
        if spectrogram.bins != self.bins() {
            return Err(StftError::Geometry {
                field: "bins",
                expected: self.bins(),
                actual: spectrogram.bins,
            });
        }
        if spectrogram.hop != self.config.hop {
            return Err(StftError::Geometry {
                field: "hop",
                expected: self.config.hop,
                actual: spectrogram.hop,
            });
        }
        if spectrogram.window_size != self.config.window_size {
            return Err(StftError::Geometry {
                field: "window_size",
                expected: self.config.window_size,
                actual: spectrogram.window_size,
            });
        }
        let expected = self.frame_count(out_len);
        if spectrogram.frames != expected {
            return Err(StftError::Geometry {
                field: "frames",
                expected,
                actual: spectrogram.frames,
            });
        }
        Ok(())
    }

    /// Window `frame` into `scratch` and transform. Lengths are guaranteed by
    /// the callers (`analyze_frame` validates; `analyze_into` constructs).
    fn analyze_windowed(&mut self, frame: &[f32], out: &mut [Complex]) {
        debug_assert_eq!(frame.len(), self.config.window_size);
        debug_assert_eq!(out.len(), self.bins());
        for ((slot, &value), &w) in self.scratch.iter_mut().zip(frame).zip(&self.window) {
            *slot = value * w;
        }
        self.fft.forward(&self.scratch, out);
    }

    /// Transform `bins` and apply the synthesis window. The overlap-add gain
    /// is applied later, once per summed sample.
    fn synthesize_weighted(&mut self, bins: &[Complex], out: &mut [f32]) {
        debug_assert_eq!(bins.len(), self.bins());
        debug_assert_eq!(out.len(), self.config.window_size);
        self.fft.inverse(bins, out);
        for (value, &w) in out.iter_mut().zip(&self.window) {
            *value *= w;
        }
    }
}

/// Periodic Hann window.
fn periodic_hann(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| {
            let angle = 2.0 * PI_F64 * i as f64 / n as f64;
            (0.5 - 0.5 * angle.cos()) as f32
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------

/// Input samples buffered per internal block while streaming, so one large
/// `process_chunk` call does not retain the whole slice.
const STREAM_BLOCK: usize = 4096;

/// Streaming STFT that consumes arbitrary-sized blocks and emits
/// sample-aligned output.
///
/// Construct with [`new`](StftStream::new), feed with
/// [`process_chunk`](StftStream::process_chunk) (or
/// [`process_chunk_identity`](StftStream::process_chunk_identity)) and finish
/// with [`flush_into`](StftStream::flush_into) /
/// [`flush_identity_into`](StftStream::flush_identity_into). The output is
/// bit-identical to [`Stft::roundtrip`] regardless of chunk sizes.
///
/// # Delay
///
/// [`latency_samples`](StftStream::latency_samples) is exactly `N`. The first
/// output sample is produced once `N` input samples have been fed; until then
/// `process_chunk` appends nothing. In steady state one hop of input produces
/// one hop of output, and `flush_into` emits the final `N − H` samples (plus
/// any remainder). The total number of emitted samples equals the number fed.
///
/// # Spectral modification
///
/// The generic methods take a callback invoked once per completed frame,
/// **after** analysis and **before** synthesis:
///
/// ```
/// use rasp_dsp::stft::{StftConfig, StftStream};
///
/// let mut stream = StftStream::new(StftConfig::default()).unwrap();
/// let input = vec![0.0f32; 4096];
/// let mut output = Vec::new();
/// // Bins are handed out in place; mutate them to build the phase vocoder.
/// stream.process_chunk(&input, &mut output, |_frame_index, _bins| {});
/// stream.flush_into(&mut output, |_frame_index, _bins| {});
/// assert_eq!(output.len(), input.len());
/// ```
pub struct StftStream {
    stft: Stft,
    /// Buffered input, starting at absolute input index `input_start`.
    input: Vec<f32>,
    input_start: u64,
    /// Total input samples received since the last reset.
    received: u64,
    /// Index of the next frame to analyse.
    next_frame: u64,
    /// Overlap-add accumulator, starting at padded index `ola_start`.
    ola: Vec<f32>,
    ola_start: u64,
    /// Next padded output index to emit (starts at `N`: input sample 0).
    next_emit: u64,
    /// Scratch for assembling a padded frame, `N` samples.
    frame: Vec<f32>,
    /// Scratch for one frame's bins.
    bins: Vec<Complex>,
    /// Scratch for one synthesized frame, `N` samples.
    time: Vec<f32>,
}

impl fmt::Debug for StftStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StftStream")
            .field("config", &self.stft.config())
            .field("received", &self.received)
            .field("next_frame", &self.next_frame)
            .field("next_emit", &self.next_emit)
            .finish_non_exhaustive()
    }
}

impl StftStream {
    /// Build a streaming STFT. Same validation as [`Stft::new`].
    pub fn new(config: StftConfig) -> Result<Self, StftError> {
        let stft = Stft::new(config)?;
        let n = config.window_size;
        let hop = config.hop;
        let bins = stft.bins();
        Ok(StftStream {
            stft,
            // Bounded by construction: input history stays below `N + block`,
            // overlap-add below `N + block + hop`.
            input: Vec::with_capacity(n + STREAM_BLOCK),
            input_start: 0,
            received: 0,
            next_frame: 0,
            ola: Vec::with_capacity(n + STREAM_BLOCK + hop),
            ola_start: 0,
            next_emit: n as u64,
            frame: vec![0.0; n],
            bins: vec![Complex::ZERO; bins],
            time: vec![0.0; n],
        })
    }

    /// Window size `N`, in samples.
    pub fn window_size(&self) -> usize {
        self.stft.window_size()
    }

    /// Hop `H = N/4`, in samples.
    pub fn hop(&self) -> usize {
        self.stft.hop()
    }

    /// Streaming lookahead in samples: exactly `N`.
    pub fn latency_samples(&self) -> usize {
        self.stft.latency_samples()
    }

    /// [`latency_samples`](StftStream::latency_samples) in seconds, or `0.0`
    /// when `sample_rate` is zero.
    pub fn latency_seconds(&self, sample_rate: u32) -> f32 {
        self.stft.latency_seconds(sample_rate)
    }

    /// Feed the next block of a stream, invoking `transform` on every frame
    /// that becomes complete.
    ///
    /// Output samples are appended to `output` as soon as they are final.
    /// `transform` receives the frame index (0-based, from the start of the
    /// stream) and the frame's mutable bins. Panicking or discarding bins is
    /// allowed; it just changes the reconstruction.
    pub fn process_chunk<F>(&mut self, input: &[f32], output: &mut Vec<f32>, mut transform: F)
    where
        F: FnMut(usize, &mut [Complex]),
    {
        for block in input.chunks(STREAM_BLOCK) {
            self.input.extend_from_slice(block);
            self.received += block.len() as u64;
            self.analyse_ready_frames(&mut transform);
            self.emit_available(output);
            self.prune();
        }
    }

    /// Finish the stream: analyse the remaining (zero-padded) frames, append
    /// the rest of the output, then reset so the instance can be reused.
    pub fn flush_into<F>(&mut self, output: &mut Vec<f32>, mut transform: F)
    where
        F: FnMut(usize, &mut [Complex]),
    {
        let total_frames = self.stft.frame_count(self.received as usize) as u64;
        while self.next_frame < total_frames {
            self.process_next_frame(&mut transform);
        }
        self.emit_available(output);
        self.reset();
    }

    /// [`process_chunk`](StftStream::process_chunk) with no spectral
    /// modification: analysis followed by exact identity synthesis.
    pub fn process_chunk_identity(&mut self, input: &[f32], output: &mut Vec<f32>) {
        self.process_chunk(input, output, |_, _| {});
    }

    /// [`flush_into`](StftStream::flush_into) with no spectral modification.
    pub fn flush_identity_into(&mut self, output: &mut Vec<f32>) {
        self.flush_into(output, |_, _| {});
    }

    /// Discard all stream state, including any unflushed input.
    pub fn reset(&mut self) {
        self.input.clear();
        self.input_start = 0;
        self.received = 0;
        self.next_frame = 0;
        self.ola.clear();
        self.ola_start = 0;
        self.next_emit = self.stft.window_size() as u64;
    }

    fn analyse_ready_frames<F>(&mut self, transform: &mut F)
    where
        F: FnMut(usize, &mut [Complex]),
    {
        let hop = self.stft.hop() as u64;
        while self.received >= self.next_frame.saturating_mul(hop) {
            self.process_next_frame(transform);
        }
    }

    fn process_next_frame<F>(&mut self, transform: &mut F)
    where
        F: FnMut(usize, &mut [Complex]),
    {
        let index = self.next_frame;
        let hop = self.stft.hop() as u64;
        let origin = index.saturating_mul(hop);
        self.fill_frame(origin);
        self.stft.analyze_windowed(&self.frame, &mut self.bins);
        transform(index as usize, &mut self.bins);
        self.stft.synthesize_weighted(&self.bins, &mut self.time);
        // Move the scratch out so `add_to_ola` can borrow the stream mutably.
        let time = core::mem::take(&mut self.time);
        self.add_to_ola(origin, &time);
        self.time = time;
        self.next_frame += 1;
    }

    /// Assemble the `N`-sample frame starting at padded index `origin`,
    /// zero-filling before the input and past the samples received so far.
    fn fill_frame(&mut self, origin: u64) {
        let n = self.stft.window_size();
        let first = origin as i64 - n as i64;
        let input_start = self.input_start;
        let received = self.received;
        let input = &self.input;
        for (i, slot) in self.frame.iter_mut().enumerate() {
            let index = first + i as i64;
            *slot = if index < 0 {
                0.0
            } else {
                let index = index as u64;
                if index < input_start || index >= received {
                    0.0
                } else {
                    input
                        .get((index - input_start) as usize)
                        .copied()
                        .unwrap_or(0.0)
                }
            };
        }
    }

    fn add_to_ola(&mut self, origin: u64, time: &[f32]) {
        // By construction frames arrive in increasing origin and no frame is
        // dropped before its window has been fully accumulated. Use a checked
        // conversion anyway: a broken invariant must not panic in release.
        let Ok(start) = usize::try_from(origin.saturating_sub(self.ola_start)) else {
            return;
        };
        let end = start.saturating_add(time.len());
        if self.ola.len() < end {
            self.ola.resize(end, 0.0);
        }
        for (dst, &value) in self.ola[start..end].iter_mut().zip(time) {
            *dst += value;
        }
    }

    fn emit_available(&mut self, output: &mut Vec<f32>) {
        let limit = self.received.saturating_add(self.stft.window_size() as u64);
        let hop = self.stft.hop() as u64;
        let gain = self.stft.inv_cola;

        loop {
            let index = self.next_emit;
            // Only emit samples inside the received input, and only once every
            // frame covering them has been accumulated.
            if index >= limit || self.next_frame.saturating_mul(hop) <= index {
                break;
            }
            let offset = index.saturating_sub(self.ola_start) as usize;
            let value = self.ola.get(offset).copied().unwrap_or(0.0);
            output.push(value * gain);
            self.next_emit += 1;
        }
    }

    /// Drop input and overlap-add samples that no future frame can touch.
    fn prune(&mut self) {
        let n = self.stft.window_size() as u64;
        let hop = self.stft.hop() as u64;

        let keep_input = self
            .next_frame
            .saturating_mul(hop)
            .saturating_sub(n)
            .min(self.received);
        if keep_input > self.input_start {
            let drop = (keep_input - self.input_start).min(self.input.len() as u64) as usize;
            self.input.drain(..drop);
            self.input_start += drop as u64;
        }

        // The overlap-add prefix may only be dropped up to the origin of the
        // next *unprocessed* frame: frames that have not been added yet still
        // start below `next_emit` (a frame origin is a window start, not its
        // centre), and their windows reach forward into the emitted region.
        let horizon = self.next_emit.min(self.next_frame.saturating_mul(hop));
        let drop = horizon.saturating_sub(self.ola_start) as usize;
        let drop = drop.min(self.ola.len());
        if drop > 0 {
            self.ola.drain(..drop);
            self.ola_start += drop as u64;
        }
    }
}

#[cfg(test)]
mod tests;
