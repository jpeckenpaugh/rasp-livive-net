//! Per-render records and the human/machine summaries.
//!
//! The output contract is treated as an API (PLAN P-06): every render reports
//! the same fields whether it came from `render` or `batch`, `--json` prints one
//! object (or one object containing every render) on **stdout**, and the human
//! text goes to stderr in that mode. Without `--json` the human text is on
//! stdout, so a plain `rasp render … | less` behaves as expected.
//!
//! Timings are wall-clock and therefore never written into `manifest.tsv` or any
//! audio file: a report may vary between runs, a render may not.

use std::path::{Path, PathBuf};

use rasp_dsp::{HeadroomReport, NaturalParams};

use crate::audio::OutFormat;
use crate::json::escape;

/// Everything one rendered file reports.
#[derive(Debug, Clone)]
pub struct RenderRecord {
    /// The file that was written.
    pub path: PathBuf,
    /// Bytes written.
    pub bytes: u64,
    /// Output container/codec.
    pub format: OutFormat,
    /// Fixture name in batch mode.
    pub fixture: Option<String>,
    /// Corpus language of the clip in batch mode (`en`, `zh`, …).
    pub language: Option<String>,
    /// `natural`, a preset id, or `custom` when an override changed the point.
    pub label: String,
    /// The nominal operating point (the preset / knob values).
    pub params: NaturalParams,
    /// The D22 headroom mapping actually applied to this render.
    pub headroom: HeadroomReport,
    /// Whether the P-08 loudness stage ran.
    pub loudness: bool,
    /// Whether endpoint trimming ran before the render.
    pub trim: bool,
    /// Samples of the input (after trimming, if any).
    pub samples_in: usize,
    /// Samples in the source before trimming.
    pub source_samples: usize,
    /// Samples of the output.
    pub samples_out: usize,
    /// Sample rate of both signals.
    pub sample_rate: u32,
    /// Sample peak of the output, dBFS.
    pub peak_dbfs: f32,
    /// 4× true peak of the output, dBTP.
    pub true_peak_dbtp: f32,
    /// Whole-clip RMS of the source, dBFS.
    pub source_rms_dbfs: f32,
    /// Whole-clip RMS of the output, dBFS.
    pub output_rms_dbfs: f32,
    /// `output − source` RMS, dB. Magnitude is the A/B level jump.
    pub level_delta_db: f32,
    /// Smallest limiter gain, dB (0 = the limiter was a no-op).
    pub limiter_min_gain_db: f32,
    /// Decode time, milliseconds.
    pub decode_ms: f64,
    /// DSP time (transform + loudness), milliseconds.
    pub render_ms: f64,
    /// Encode + write time, milliseconds.
    pub write_ms: f64,
}

impl RenderRecord {
    /// Duration of the rendered audio, seconds.
    pub fn duration_s(&self) -> f64 {
        if self.sample_rate == 0 {
            0.0
        } else {
            self.samples_out as f64 / f64::from(self.sample_rate)
        }
    }

    /// Decode + render + write, milliseconds.
    pub fn total_ms(&self) -> f64 {
        self.decode_ms + self.render_ms + self.write_ms
    }

    /// Total milliseconds per second of audio.
    pub fn ms_per_audio_second(&self) -> f64 {
        let seconds = self.duration_s();
        if seconds > 0.0 {
            self.total_ms() / seconds
        } else {
            0.0
        }
    }

    /// DSP milliseconds per second of audio (the P-05 comparable number).
    pub fn render_ms_per_audio_second(&self) -> f64 {
        let seconds = self.duration_s();
        if seconds > 0.0 {
            self.render_ms / seconds
        } else {
            0.0
        }
    }

    /// The JSON object for this record.
    pub fn to_json(&self) -> String {
        let mut out = String::from("{");
        push_field(&mut out, "\"file\"", &escape(&display(&self.path)));
        out.push(',');
        push_field(
            &mut out,
            "\"fixture\"",
            &match &self.fixture {
                Some(name) => escape(name),
                None => "null".to_string(),
            },
        );
        out.push(',');
        push_field(
            &mut out,
            "\"language\"",
            &match &self.language {
                Some(language) => escape(language),
                None => "null".to_string(),
            },
        );
        out.push(',');
        push_field(&mut out, "\"preset\"", &escape(&self.label));
        out.push(',');
        out.push_str("\"params\":{");
        out.push_str(&format!(
            "\"pitchSt\":{:.6},\"formantRatio\":{:.6},\"tiltDbPerOct\":{:.6}",
            f64::from(self.params.pitch_st),
            f64::from(self.params.formant_ratio),
            f64::from(self.params.tilt_db_per_oct)
        ));
        out.push('}');
        out.push(',');
        out.push_str("\"headroom\":{");
        out.push_str(&format!(
            "\"mode\":\"{}\",\"weight\":{:.6},\"measuredF0Hz\":{},\"effectivePitchSt\":{:.6},\"effectiveFormantRatio\":{:.6},\"effectiveTiltDbPerOct\":{:.6}",
            self.headroom.mode.id(),
            f64::from(self.headroom.weight),
            match self.headroom.measured_f0_hz {
                Some(hz) => format!("{:.4}", f64::from(hz)),
                None => "null".to_string(),
            },
            f64::from(self.headroom.effective_pitch_st()),
            f64::from(self.headroom.effective_formant_ratio()),
            f64::from(self.headroom.effective_tilt_db_per_oct()),
        ));
        out.push('}');
        out.push(',');
        push_field(&mut out, "\"format\"", &escape(self.format.id()));
        out.push(',');
        out.push_str(&format!("\"loudness\":{}", self.loudness));
        out.push(',');
        out.push_str(&format!("\"trim\":{}", self.trim));
        out.push(',');
        out.push_str(&format!("\"sampleRate\":{}", self.sample_rate));
        out.push_str(",\"channels\":1");
        out.push_str(&format!(",\"sourceSamples\":{}", self.source_samples));
        out.push_str(&format!(",\"samplesIn\":{}", self.samples_in));
        out.push_str(&format!(",\"samplesOut\":{}", self.samples_out));
        out.push_str(&format!(",\"durationS\":{:.6}", self.duration_s()));
        out.push_str(&format!(",\"bytes\":{}", self.bytes));
        out.push_str(&format!(",\"peakDbfs\":{:.4}", f64::from(self.peak_dbfs)));
        out.push_str(&format!(
            ",\"truePeakDbtp\":{:.4}",
            f64::from(self.true_peak_dbtp)
        ));
        out.push_str(&format!(
            ",\"sourceRmsDbfs\":{:.4}",
            f64::from(self.source_rms_dbfs)
        ));
        out.push_str(&format!(
            ",\"outputRmsDbfs\":{:.4}",
            f64::from(self.output_rms_dbfs)
        ));
        out.push_str(&format!(
            ",\"levelDeltaDb\":{:.4}",
            f64::from(self.level_delta_db)
        ));
        out.push_str(&format!(
            ",\"limiterMinGainDb\":{:.4}",
            f64::from(self.limiter_min_gain_db)
        ));
        out.push_str(&format!(",\"decodeMs\":{:.3}", self.decode_ms));
        out.push_str(&format!(",\"renderMs\":{:.3}", self.render_ms));
        out.push_str(&format!(",\"writeMs\":{:.3}", self.write_ms));
        out.push_str(&format!(",\"totalMs\":{:.3}", self.total_ms()));
        out.push_str(&format!(
            ",\"msPerAudioSecond\":{:.3}",
            self.ms_per_audio_second()
        ));
        out.push('}');
        out
    }

    /// The human block for `rasp render` (multi-line, no trailing newline).
    pub fn to_human(&self) -> String {
        let params = self.params;
        let mut text = String::new();
        text.push_str(&format!(
            "input   {} samples ({:.3} s){}\n",
            self.source_samples,
            self.source_samples as f64 / f64::from(self.sample_rate.max(1)),
            if self.trim {
                format!(" -> trimmed to {} samples", self.samples_in)
            } else {
                String::new()
            }
        ));
        text.push_str(&format!(
            "params  preset={}  pitch={:+.2} st  formant={:.3}x  tilt={:+.2} dB/oct  loudness={}  trim={}\n",
            self.label,
            params.pitch_st,
            params.formant_ratio,
            params.tilt_db_per_oct,
            on_off(self.loudness),
            on_off(self.trim)
        ));
        text.push_str(&format!(
            "headroom w={:.3}  f0={}  -> pitch {:+.2} st  formant {:.3}x  tilt {:+.2} dB/oct{}  mode={}\n",
            self.headroom.weight,
            match self.headroom.measured_f0_hz {
                Some(hz) => format!("{hz:.1} Hz"),
                None => "unmeasured".to_string(),
            },
            self.headroom.effective_pitch_st(),
            self.headroom.effective_formant_ratio(),
            self.headroom.effective_tilt_db_per_oct(),
            if self.headroom.no_headroom() {
                "  (no headroom)"
            } else {
                ""
            },
            self.headroom.mode.id(),
        ));
        text.push_str(&format!(
            "out     {}  {} samples ({:.3} s)  {}  {} B\n",
            display(&self.path),
            self.samples_out,
            self.duration_s(),
            self.format.id(),
            self.bytes
        ));
        text.push_str(&format!(
            "levels  peak {:+.2} dBFS  true peak {:+.2} dBTP  A/B delta {:+.2} dB  limiter {:+.2} dB\n",
            self.peak_dbfs, self.true_peak_dbtp, self.level_delta_db, self.limiter_min_gain_db
        ));
        text.push_str(&format!(
            "timing  decode {:.2} ms  render {:.2} ms  write {:.2} ms  total {:.2} ms = {:.2} ms per audio second (render {:.2} ms/s)",
            self.decode_ms,
            self.render_ms,
            self.write_ms,
            self.total_ms(),
            self.ms_per_audio_second(),
            self.render_ms_per_audio_second()
        ));
        text
    }

    /// The one-line form used inside a batch.
    pub fn to_batch_line(&self) -> String {
        format!(
            "{:<28} {:<10} in {:>7} out {:>7}  peak {:+6.2}  tp {:+6.2}  d {:+.2} dB  w {:.2}  {:>7.2} ms  -> {}",
            self.fixture.as_deref().unwrap_or("-"),
            self.label,
            self.samples_in,
            self.samples_out,
            self.peak_dbfs,
            self.true_peak_dbtp,
            self.level_delta_db,
            self.headroom.weight,
            self.render_ms,
            display(&self.path)
        )
    }
}

/// Aggregate numbers for a batch, including the P-08 and P-06 gates.
#[derive(Debug, Clone, Default)]
pub struct Summary {
    /// Renders written.
    pub files: usize,
    /// Audio seconds written.
    pub audio_seconds: f64,
    /// Summed decode time, milliseconds.
    pub decode_ms: f64,
    /// Summed DSP time, milliseconds.
    pub render_ms: f64,
    /// Summed encode/write time, milliseconds.
    pub write_ms: f64,
    /// Loudest sample peak across renders, dBFS.
    pub worst_peak_dbfs: f32,
    /// Highest true peak across renders, dBTP.
    pub worst_true_peak_dbtp: f32,
    /// Largest `|level delta|` across renders, dB.
    pub worst_level_delta_db: f32,
}

impl Summary {
    /// Build a summary from records.
    pub fn from_records(records: &[RenderRecord]) -> Self {
        let mut summary = Summary {
            files: records.len(),
            worst_peak_dbfs: f32::NEG_INFINITY,
            worst_true_peak_dbtp: f32::NEG_INFINITY,
            worst_level_delta_db: 0.0,
            ..Summary::default()
        };
        for record in records {
            summary.audio_seconds += record.duration_s();
            summary.decode_ms += record.decode_ms;
            summary.render_ms += record.render_ms;
            summary.write_ms += record.write_ms;
            summary.worst_peak_dbfs = summary.worst_peak_dbfs.max(record.peak_dbfs);
            summary
                .worst_true_peak_dbtp
                .max_assign(record.true_peak_dbtp);
            summary
                .worst_level_delta_db
                .max_assign(record.level_delta_db.abs());
        }
        if records.is_empty() {
            summary.worst_peak_dbfs = f32::NEG_INFINITY;
            summary.worst_true_peak_dbtp = f32::NEG_INFINITY;
        }
        summary
    }

    /// Total milliseconds per audio second, including decode and write.
    pub fn ms_per_audio_second(&self) -> f64 {
        if self.audio_seconds > 0.0 {
            (self.decode_ms + self.render_ms + self.write_ms) / self.audio_seconds
        } else {
            0.0
        }
    }

    /// DSP milliseconds per audio second.
    pub fn render_ms_per_audio_second(&self) -> f64 {
        if self.audio_seconds > 0.0 {
            self.render_ms / self.audio_seconds
        } else {
            0.0
        }
    }

    /// The human summary line.
    pub fn to_human(&self) -> String {
        format!(
            "{} renders, {:.3} s of audio: total {:.1} ms = {:.2} ms/s (decode {:.2} ms/s, render {:.2} ms/s, write {:.2} ms/s)\n\
             worst peak {:+.2} dBFS, worst true peak {:+.2} dBTP, worst |A/B delta| {:.2} dB",
            self.files,
            self.audio_seconds,
            self.decode_ms + self.render_ms + self.write_ms,
            self.ms_per_audio_second(),
            self.decode_ms / self.audio_seconds.max(f64::MIN_POSITIVE),
            self.render_ms_per_audio_second(),
            self.write_ms / self.audio_seconds.max(f64::MIN_POSITIVE),
            self.worst_peak_dbfs,
            self.worst_true_peak_dbtp,
            self.worst_level_delta_db
        )
    }

    /// The summary's JSON object (no outer braces, no trailing comma).
    pub fn to_json_fields(&self) -> String {
        format!(
            "\"files\":{},\"audioSeconds\":{:.6},\"decodeMs\":{:.3},\"renderMs\":{:.3},\"writeMs\":{:.3},\"totalMs\":{:.3},\"msPerAudioSecond\":{:.3},\"renderMsPerAudioSecond\":{:.3},\"worstPeakDbfs\":{:.4},\"worstTruePeakDbtp\":{:.4},\"worstAbsLevelDeltaDb\":{:.4}",
            self.files,
            self.audio_seconds,
            self.decode_ms,
            self.render_ms,
            self.write_ms,
            self.decode_ms + self.render_ms + self.write_ms,
            self.ms_per_audio_second(),
            self.render_ms_per_audio_second(),
            f64::from(self.worst_peak_dbfs),
            f64::from(self.worst_true_peak_dbtp),
            f64::from(self.worst_level_delta_db)
        )
    }
}

/// Small extension so `f32::max` can be used with `max_assign` without
/// importing `PartialOrd` tricks at every call site.
trait MaxAssign {
    fn max_assign(&mut self, other: Self);
}

impl MaxAssign for f32 {
    fn max_assign(&mut self, other: f32) {
        if other > *self {
            *self = other;
        }
    }
}

fn push_field(out: &mut String, name: &str, value: &str) {
    out.push_str(name);
    out.push(':');
    out.push_str(value);
}

fn on_off(value: bool) -> &'static str {
    if value {
        "on"
    } else {
        "off"
    }
}

/// Display a path without forcing it through `to_string_lossy` at each call
/// site. Non-UTF-8 paths still print something stable.
pub fn display(path: &Path) -> String {
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> RenderRecord {
        RenderRecord {
            path: PathBuf::from("/tmp/x.wav"),
            bytes: 100,
            format: OutFormat::Wav16,
            fixture: Some("a__b".to_string()),
            language: Some("en".to_string()),
            label: "deep".to_string(),
            params: NaturalParams::new(-4.0, 0.85, 0.0),
            headroom: HeadroomReport::new(
                rasp_dsp::HeadroomMode::On,
                NaturalParams::new(-4.0, 0.85, 0.0),
                Some(164.08),
            ),
            loudness: true,
            trim: false,
            samples_in: 48_000,
            source_samples: 48_000,
            samples_out: 48_000,
            sample_rate: 48_000,
            peak_dbfs: -2.0,
            true_peak_dbtp: -1.5,
            source_rms_dbfs: -15.0,
            output_rms_dbfs: -15.1,
            level_delta_db: -0.1,
            limiter_min_gain_db: 0.0,
            decode_ms: 0.5,
            render_ms: 30.0,
            write_ms: 0.5,
        }
    }

    #[test]
    fn json_is_well_formed_and_stable() {
        let record = record();
        let text = record.to_json();
        let parsed = crate::json::parse(&text).expect("valid JSON object");
        assert_eq!(
            parsed.get("preset").and_then(crate::json::Value::as_str),
            Some("deep")
        );
        assert_eq!(
            parsed.get("fixture").and_then(crate::json::Value::as_str),
            Some("a__b")
        );
        assert_eq!(text, record.to_json());
        assert!(record.to_human().contains("render"));
        assert!(record.to_batch_line().contains("deep"));
    }

    #[test]
    fn summary_reports_worst_cases_and_rates() {
        let mut first = record();
        let mut second = record();
        second.level_delta_db = -0.5;
        second.true_peak_dbtp = -1.01;
        first.level_delta_db = 0.2;
        let summary = Summary::from_records(&[first, second]);
        assert_eq!(summary.files, 2);
        assert!((summary.worst_level_delta_db - 0.5).abs() < 1e-6);
        assert!((summary.worst_true_peak_dbtp + 1.01).abs() < 1e-6);
        assert!((summary.ms_per_audio_second() - 31.0).abs() < 1e-6);
        let json = format!("{{{}}}", summary.to_json_fields());
        crate::json::parse(&json).expect("valid JSON");
    }
}
