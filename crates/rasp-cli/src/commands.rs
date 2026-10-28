//! The four commands: `render`, `batch`, `decode`, `info`.
//!
//! All commands share the same contract:
//!
//! * input is 48 kHz mono `.weba`, raw `.f32`, or `.wav` (see [`crate::audio`]);
//! * a failure prints exactly one line and exits non-zero — no panic, no
//!   backtrace, no `unwrap` on anything the user typed;
//! * `--json` writes a single machine-readable object to stdout and the human
//!   summary to stderr, so a script can consume stdout while a human still sees
//!   progress.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use rasp_dsp::{
    dbfs, detect_endpoints, rms_dbfs, sample_peak, trim_clip, true_peak_dbtp, EndpointConfig,
    LoudnessConfig, LoudnessReport, NaturalParams, NaturalPreset, NaturalRenderer,
    VoiceTransformConfig, VoiceTransformer, MAX_FORMANT_RATIO, MAX_PITCH_ST, MAX_TILT_DB_PER_OCT,
    MIN_FORMANT_RATIO, MIN_PITCH_ST, MIN_TILT_DB_PER_OCT,
};

use crate::args::{self, Parsed, Spec};
use crate::audio::{self, OutFormat, SAMPLE_RATE};
use crate::json::{self, escape, Value};
use crate::report::{self, RenderRecord, Summary};
use crate::{CliError, Result};

// ---------------------------------------------------------------------------
// Option specs
// ---------------------------------------------------------------------------

const RENDER_VALUES: &[&str] = &[
    "--in",
    "--out",
    "--out-dir",
    "--preset",
    "--pitch",
    "--formant",
    "--tilt",
    "--format",
    "--loudness",
];
const RENDER_FLAGS: &[&str] = &["--trim", "--json"];

const BATCH_VALUES: &[&str] = &[
    "--manifest",
    "--voice",
    "--phrase",
    "--presets",
    "--out-dir",
    "--format",
    "--loudness",
    "--input",
];
const BATCH_FLAGS: &[&str] = &["--all", "--trim", "--json"];

const DECODE_VALUES: &[&str] = &["--in", "--out", "--out-dir", "--format"];
const DECODE_FLAGS: &[&str] = &["--json"];

const INFO_VALUES: &[&str] = &["--in"];
const INFO_FLAGS: &[&str] = &["--json"];

/// The default fixture manifest, relative to the working directory.
const DEFAULT_MANIFEST: &str = "fixtures/manifest.json";
/// The default batch output directory.
const DEFAULT_BATCH_DIR: &str = "rasp-renders";

// ---------------------------------------------------------------------------
// render
// ---------------------------------------------------------------------------

/// `rasp render`
pub fn render(tokens: &[String]) -> Result<()> {
    if args::wants_help(tokens) {
        println!("{RENDER_HELP}");
        return Ok(());
    }
    let parsed = args::parse(Spec::new(RENDER_VALUES, RENDER_FLAGS), tokens)?;

    let input_path = PathBuf::from(parsed.require("--in")?);
    let out = parsed.value("--out").map(PathBuf::from);
    let out_dir = parsed.value("--out-dir").map(PathBuf::from);

    let choice = resolve_params(&parsed)?;
    let format = parse_format(&parsed, OutFormat::Wav16)?;
    let loudness = parse_loudness(&parsed, true)?;
    let trim = parsed.flag("--trim");
    let as_json = parsed.flag("--json");

    let out_name = format!(
        "{}.{}.{}",
        file_stem(&input_path),
        choice.label,
        format.extension()
    );
    let out_path = resolve_output(out, out_dir, &out_name)?;

    let loaded = timed_load(&input_path)?;
    let source_samples = loaded.input.samples.len();
    let samples = if trim {
        let trimmed = trim_clip(
            &loaded.input.samples,
            SAMPLE_RATE,
            &EndpointConfig::corpus(),
        );
        if trimmed.is_empty() {
            return Err(CliError::failed(format!(
                "{}: endpoint detection would remove the whole clip",
                input_path.display()
            )));
        }
        trimmed.samples
    } else {
        loaded.input.samples
    };
    let samples_in = samples.len();

    let rendered = render_samples(&samples, choice.params, loudness)?;

    let write_start = Instant::now();
    let bytes = audio::encode(&rendered.samples, format)?;
    write_file(&out_path, &bytes)?;
    let write_ms = elapsed_ms(write_start);

    let record = build_record(BuildArgs {
        path: out_path,
        bytes: bytes.len() as u64,
        format,
        fixture: None,
        language: None,
        label: choice.label,
        params: choice.params,
        loudness,
        trim,
        source_samples,
        samples_in,
        samples_out: rendered.samples.len(),
        rendered: &rendered,
        decode_ms: loaded.decode_ms,
        write_ms,
    });
    emit_render(&record, as_json);
    Ok(())
}

// ---------------------------------------------------------------------------
// batch
// ---------------------------------------------------------------------------

/// `rasp batch`
pub fn batch(tokens: &[String]) -> Result<()> {
    if args::wants_help(tokens) {
        println!("{BATCH_HELP}");
        return Ok(());
    }
    let parsed = args::parse(Spec::new(BATCH_VALUES, BATCH_FLAGS), tokens)?;

    let all = parsed.flag("--all");
    let voice = parsed.value("--voice");
    let phrase = parsed.value("--phrase");
    if all && (voice.is_some() || phrase.is_some()) {
        return Err(CliError::usage(
            "--all selects every clip and cannot be combined with --voice/--phrase",
        ));
    }
    if parsed.value("--manifest").is_none() && !all && voice.is_none() && phrase.is_none() {
        return Err(CliError::usage(
            "batch needs a selection: --manifest <file>, --voice/--phrase, or --all (see --help)",
        ));
    }

    let manifest_path = parsed
        .value("--manifest")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_MANIFEST));
    let clips = load_manifest(&manifest_path)?;
    let selected: Vec<&ClipEntry> = clips
        .iter()
        .filter(|clip| voice.is_none_or(|want| clip.voice == want))
        .filter(|clip| phrase.is_none_or(|want| clip.phrase == want))
        .collect();
    if selected.is_empty() {
        return Err(CliError::failed(format!(
            "no clips in {} match {}",
            manifest_path.display(),
            selection_text(voice, phrase)
        )));
    }

    let presets = parse_presets(&parsed)?;
    let format = parse_format(&parsed, OutFormat::RawF32)?;
    let loudness = parse_loudness(&parsed, true)?;
    let trim = parsed.flag("--trim");
    let as_json = parsed.flag("--json");
    let input_pref = parse_input_pref(&parsed)?;
    let out_dir = parsed
        .value("--out-dir")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_BATCH_DIR));

    fs::create_dir_all(&out_dir).map_err(|err| {
        CliError::failed(format!(
            "cannot create output directory {}: {err}",
            out_dir.display()
        ))
    })?;

    let mut records: Vec<RenderRecord> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    let mut table = String::from(
        "fixture\tstep\tcents\tratio\tpreset\tpitch_st\tformant\ttilt_db_per_oct\tfile\tsamples\tpeak_dbfs\ttrue_peak_dbtp\tlevel_delta_db\tlanguage\n",
    );

    for clip in &selected {
        let source = clip.input_path(input_pref, &manifest_path)?;
        for preset in &presets {
            let params = preset.params();
            match batch_one(BatchArgs {
                clip,
                source: &source,
                params,
                label: preset.id(),
                format,
                loudness,
                trim,
                out_dir: &out_dir,
            }) {
                Ok(record) => {
                    table.push_str(&manifest_row(&record));
                    if !as_json {
                        println!("{}", record.to_batch_line());
                    }
                    records.push(record);
                }
                Err(err) => {
                    let message = err.message().to_string();
                    eprintln!("rasp: {message}");
                    failures.push(message);
                }
            }
        }
    }

    let manifest_out = out_dir.join("manifest.tsv");
    write_file(&manifest_out, table.as_bytes())?;
    let summary = Summary::from_records(&records);

    if as_json {
        let mut json = String::from("{\"ok\":");
        json.push_str(if failures.is_empty() { "true" } else { "false" });
        json.push_str(",\"manifest\":");
        json.push_str(&escape(&report::display(&manifest_out)));
        json.push_str(",\"format\":");
        json.push_str(&escape(format.id()));
        json.push_str(",\"renders\":[");
        for (index, record) in records.iter().enumerate() {
            if index > 0 {
                json.push(',');
            }
            json.push_str(&record.to_json());
        }
        json.push_str("],\"summary\":{");
        json.push_str(&summary.to_json_fields());
        json.push_str("},\"failures\":[");
        for (index, failure) in failures.iter().enumerate() {
            if index > 0 {
                json.push(',');
            }
            json.push_str(&escape(failure));
        }
        json.push_str("]}");
        println!("{json}");
        eprintln!("{}", summary.to_human());
        eprintln!("manifest: {}", manifest_out.display());
    } else {
        println!("{}", summary.to_human());
        println!("manifest: {}", manifest_out.display());
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(CliError::failed(format!(
            "{} of {} renders failed ({} written; manifest lists only successes)",
            failures.len(),
            selected.len() * presets.len(),
            records.len()
        )))
    }
}

struct BatchArgs<'a> {
    clip: &'a ClipEntry,
    source: &'a Path,
    params: NaturalParams,
    label: &'a str,
    format: OutFormat,
    loudness: bool,
    trim: bool,
    out_dir: &'a Path,
}

fn batch_one(args: BatchArgs<'_>) -> Result<RenderRecord> {
    let loaded = timed_load(args.source)?;
    if loaded.input.samples.len() != args.clip.sample_count {
        return Err(CliError::failed(format!(
            "{}: decoded {} samples, manifest says {} (a framing or fixture mismatch)",
            args.source.display(),
            loaded.input.samples.len(),
            args.clip.sample_count
        )));
    }
    let source_samples = loaded.input.samples.len();
    let samples = if args.trim {
        let trimmed = trim_clip(
            &loaded.input.samples,
            SAMPLE_RATE,
            &EndpointConfig::corpus(),
        );
        if trimmed.is_empty() {
            return Err(CliError::failed(format!(
                "{}: endpoint detection would remove the whole clip",
                args.source.display()
            )));
        }
        trimmed.samples
    } else {
        loaded.input.samples
    };
    let samples_in = samples.len();
    let rendered = render_samples(&samples, args.params, args.loudness)?;

    let path = args.out_dir.join(format!(
        "{}.{}.{}",
        args.clip.name,
        args.label,
        args.format.extension()
    ));
    let write_start = Instant::now();
    let bytes = audio::encode(&rendered.samples, args.format)?;
    write_file(&path, &bytes)?;
    let write_ms = elapsed_ms(write_start);

    Ok(build_record(BuildArgs {
        path,
        bytes: bytes.len() as u64,
        format: args.format,
        fixture: Some(args.clip.name.clone()),
        language: Some(args.clip.language.clone()),
        label: args.label.to_string(),
        params: args.params,
        loudness: args.loudness,
        trim: args.trim,
        source_samples,
        samples_in,
        samples_out: rendered.samples.len(),
        rendered: &rendered,
        decode_ms: loaded.decode_ms,
        write_ms,
    }))
}

/// The columns `scripts/metrics.py --renders --manifest` consumes, plus the
/// P-08 report fields. `ratio` is the **pitch** ratio (metrics' meaning);
/// the formant ratio is `formant`.
fn manifest_row(record: &RenderRecord) -> String {
    let params = record.params;
    let file = record
        .path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let fixture = record.fixture.as_deref().unwrap_or("");
    let language = record.language.as_deref().unwrap_or("");
    let step = params.pitch_st.round() as i32;
    format!(
        "{fixture}\t{step:+}\t{:.6}\t{:.6}\t{}\t{:.4}\t{:.4}\t{:.4}\t{file}\t{}\t{:.4}\t{:.4}\t{:.4}\t{language}\n",
        f64::from(params.pitch_st) * 100.0,
        f64::from(params.pitch_ratio()),
        record.label,
        params.pitch_st,
        params.formant_ratio,
        params.tilt_db_per_oct,
        record.samples_out,
        record.peak_dbfs,
        record.true_peak_dbtp,
        record.level_delta_db,
    )
}

// ---------------------------------------------------------------------------
// decode
// ---------------------------------------------------------------------------

/// `rasp decode`
pub fn decode(tokens: &[String]) -> Result<()> {
    if args::wants_help(tokens) {
        println!("{DECODE_HELP}");
        return Ok(());
    }
    let parsed = args::parse(Spec::new(DECODE_VALUES, DECODE_FLAGS), tokens)?;
    let input_path = PathBuf::from(parsed.require("--in")?);
    let format = parse_format(&parsed, OutFormat::RawF32)?;
    let as_json = parsed.flag("--json");
    let out = parsed.value("--out").map(PathBuf::from);
    let out_dir = parsed.value("--out-dir").map(PathBuf::from);
    let out_name = format!("{}.{}", file_stem(&input_path), format.extension());
    let out_path = resolve_output(out, out_dir, &out_name)?;

    let loaded = timed_load(&input_path)?;
    let input = &loaded.input;

    let write_start = Instant::now();
    let bytes = audio::encode(&input.samples, format)?;
    write_file(&out_path, &bytes)?;
    let write_ms = elapsed_ms(write_start);
    let duration = input.samples.len() as f64 / f64::from(input.sample_rate);

    if as_json {
        let json = format!(
            "{{\"ok\":true,\"file\":{},\"input\":{},\"sourceFormat\":{},\"format\":{},\"sampleRate\":{},\"channels\":{},\"samples\":{},\"durationS\":{:.6},\"bytes\":{},\"decodeMs\":{:.3},\"writeMs\":{:.3}}}",
            escape(&report::display(&out_path)),
            escape(&report::display(&input_path)),
            escape(input.kind),
            escape(format.id()),
            input.sample_rate,
            input.channels,
            input.samples.len(),
            duration,
            bytes.len(),
            loaded.decode_ms,
            write_ms
        );
        println!("{json}");
        eprintln!(
            "decoded {} ({}, {} samples) -> {} ({}, {} B)",
            input_path.display(),
            input.kind,
            input.samples.len(),
            out_path.display(),
            format.id(),
            bytes.len()
        );
    } else {
        println!(
            "decoded {} ({}, {} samples, {:.3} s) -> {} ({}, {} B) in {:.2} ms",
            input_path.display(),
            input.kind,
            input.samples.len(),
            duration,
            out_path.display(),
            format.id(),
            bytes.len(),
            loaded.decode_ms + write_ms
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// info
// ---------------------------------------------------------------------------

/// `rasp info`
pub fn info(tokens: &[String]) -> Result<()> {
    if args::wants_help(tokens) {
        println!("{INFO_HELP}");
        return Ok(());
    }
    let parsed = args::parse(Spec::new(INFO_VALUES, INFO_FLAGS), tokens)?;
    let input_path = PathBuf::from(parsed.require("--in")?);
    let as_json = parsed.flag("--json");
    let loaded = timed_load(&input_path)?;
    let input = &loaded.input;
    let samples = &input.samples;
    let duration = samples.len() as f64 / f64::from(input.sample_rate);
    let peak = sample_peak(samples);
    let true_peak = true_peak_dbtp(samples);
    let rms = rms_dbfs(samples);
    let endpoints = detect_endpoints(samples, input.sample_rate, &EndpointConfig::corpus());

    if as_json {
        let json = format!(
            "{{\"ok\":true,\"file\":{},\"sourceFormat\":{},\"sampleRate\":{},\"channels\":{},\"frames\":{},\"durationS\":{:.6},\"peakDbfs\":{:.4},\"truePeakDbtp\":{:.4},\"rmsDbfs\":{:.4},\"endpoints\":{{\"headMs\":{:.3},\"tailMs\":{:.3},\"startSample\":{},\"endSample\":{},\"trimmedSamples\":{}}},\"decodeMs\":{:.3}}}",
            escape(&report::display(&input_path)),
            escape(input.kind),
            input.sample_rate,
            input.channels,
            samples.len(),
            duration,
            f64::from(dbfs(peak)),
            f64::from(true_peak),
            f64::from(rms),
            endpoints.head_ms,
            endpoints.tail_ms,
            endpoints.start,
            endpoints.end,
            endpoints.len(),
            loaded.decode_ms
        );
        println!("{json}");
    } else {
        println!("file      {} ({})", input_path.display(), input.kind);
        println!(
            "format    {} Hz, {} channel(s), {} frames, {:.3} s",
            input.sample_rate,
            input.channels,
            samples.len(),
            duration
        );
        println!(
            "levels    peak {:+.2} dBFS   true peak {:+.2} dBTP   RMS {:+.2} dBFS",
            dbfs(peak),
            true_peak,
            rms
        );
        println!(
            "endpoints head {:.1} ms   tail {:.1} ms   keep [{}, {}) = {} samples",
            endpoints.head_ms,
            endpoints.tail_ms,
            endpoints.start,
            endpoints.end,
            endpoints.len()
        );
        println!("decoded in {:.2} ms", loaded.decode_ms);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared rendering
// ---------------------------------------------------------------------------

/// A loaded input plus how long loading (decode included) took.
struct Loaded {
    input: audio::Input,
    decode_ms: f64,
}

fn timed_load(path: &Path) -> Result<Loaded> {
    let start = Instant::now();
    let input = audio::load(path)?;
    Ok(Loaded {
        input,
        decode_ms: elapsed_ms(start),
    })
}

/// The result of the DSP chain, with the measurements a report needs.
struct Rendered {
    samples: Vec<f32>,
    source_rms_dbfs: f32,
    output_rms_dbfs: f32,
    level_delta_db: f32,
    limiter_min_gain_db: f32,
    peak_dbfs: f32,
    true_peak_dbtp: f32,
    render_ms: f64,
}

/// Run the natural chain (transform → optional P-08 loudness stage).
fn render_samples(input: &[f32], params: NaturalParams, loudness: bool) -> Result<Rendered> {
    let params = params.sanitised();
    let start = Instant::now();
    let mut output = Vec::with_capacity(input.len());

    if loudness {
        let mut renderer = NaturalRenderer::new(
            SAMPLE_RATE,
            VoiceTransformConfig::d2(),
            LoudnessConfig::default(),
        )
        .map_err(|err| CliError::failed(format!("cannot build the render chain: {err}")))?;
        renderer.set_params(params);
        let report: LoudnessReport = renderer
            .process_into(input, &mut output)
            .map_err(|err| CliError::failed(format!("render failed: {err}")))?;
        let render_ms = elapsed_ms(start);
        Ok(Rendered {
            samples: output,
            source_rms_dbfs: report.source_rms_dbfs,
            output_rms_dbfs: report.output_rms_dbfs,
            level_delta_db: report.level_delta_db,
            limiter_min_gain_db: report.limiter_min_gain_db,
            peak_dbfs: report.output_sample_peak_dbfs,
            true_peak_dbtp: report.output_true_peak_dbtp,
            render_ms,
        })
    } else {
        // `--loudness off` is the raw transform: no RMS match and no limiter.
        // `NaturalRenderer` always owns a loudness stage, so the off path uses
        // the transform directly with the same D2 config.
        let mut transformer = VoiceTransformer::new(SAMPLE_RATE, VoiceTransformConfig::d2())
            .map_err(|err| CliError::failed(format!("cannot build the transform: {err}")))?;
        transformer.set_pitch_semitones(params.pitch_st);
        transformer.set_formant_ratio(params.formant_ratio);
        transformer.set_tilt_db_per_oct(params.tilt_db_per_oct);
        transformer
            .process_into(input, &mut output)
            .map_err(|err| CliError::failed(format!("render failed: {err}")))?;
        let render_ms = elapsed_ms(start);
        let source_rms = rms_dbfs(input);
        let output_rms = rms_dbfs(&output);
        let peak_dbfs = dbfs(sample_peak(&output));
        let true_peak = true_peak_dbtp(&output);
        Ok(Rendered {
            samples: output,
            source_rms_dbfs: source_rms,
            output_rms_dbfs: output_rms,
            level_delta_db: output_rms - source_rms,
            limiter_min_gain_db: 0.0,
            peak_dbfs,
            true_peak_dbtp: true_peak,
            render_ms,
        })
    }
}

struct BuildArgs<'a> {
    path: PathBuf,
    bytes: u64,
    format: OutFormat,
    fixture: Option<String>,
    language: Option<String>,
    label: String,
    params: NaturalParams,
    loudness: bool,
    trim: bool,
    source_samples: usize,
    samples_in: usize,
    samples_out: usize,
    rendered: &'a Rendered,
    decode_ms: f64,
    write_ms: f64,
}

fn build_record(args: BuildArgs<'_>) -> RenderRecord {
    RenderRecord {
        path: args.path,
        bytes: args.bytes,
        format: args.format,
        fixture: args.fixture,
        language: args.language,
        label: args.label,
        params: args.params,
        loudness: args.loudness,
        trim: args.trim,
        samples_in: args.samples_in,
        source_samples: args.source_samples,
        samples_out: args.samples_out,
        sample_rate: SAMPLE_RATE,
        peak_dbfs: args.rendered.peak_dbfs,
        true_peak_dbtp: args.rendered.true_peak_dbtp,
        source_rms_dbfs: args.rendered.source_rms_dbfs,
        output_rms_dbfs: args.rendered.output_rms_dbfs,
        level_delta_db: args.rendered.level_delta_db,
        limiter_min_gain_db: args.rendered.limiter_min_gain_db,
        decode_ms: args.decode_ms,
        render_ms: args.rendered.render_ms,
        write_ms: args.write_ms,
    }
}

fn emit_render(record: &RenderRecord, as_json: bool) {
    if as_json {
        println!("{}", record.to_json());
        eprintln!("{}", record.to_human());
    } else {
        println!("{}", record.to_human());
    }
}

// ---------------------------------------------------------------------------
// Option resolution
// ---------------------------------------------------------------------------

struct Choice {
    params: NaturalParams,
    label: String,
}

/// Resolve `--preset` plus explicit overrides. The preset fills every value it
/// defines; an override wins; an override without a preset starts from
/// `natural`.
fn resolve_params(parsed: &Parsed) -> Result<Choice> {
    let preset = match parsed.value("--preset") {
        Some(id) => Some(NaturalPreset::from_id(id).ok_or_else(|| {
            CliError::usage(format!(
                "unknown preset '{id}'; expected one of: {}",
                preset_ids()
            ))
        })?),
        None => None,
    };
    let base = preset
        .map(NaturalPreset::params)
        .unwrap_or_else(NaturalParams::natural);

    let pitch = range(parsed, "--pitch", MIN_PITCH_ST, MAX_PITCH_ST)?.unwrap_or(base.pitch_st);
    let formant = range(parsed, "--formant", MIN_FORMANT_RATIO, MAX_FORMANT_RATIO)?
        .unwrap_or(base.formant_ratio);
    let tilt = range(parsed, "--tilt", MIN_TILT_DB_PER_OCT, MAX_TILT_DB_PER_OCT)?
        .unwrap_or(base.tilt_db_per_oct);

    let params = NaturalParams::new(pitch, formant, tilt);
    let overridden =
        pitch != base.pitch_st || formant != base.formant_ratio || tilt != base.tilt_db_per_oct;
    let label = match (preset, overridden) {
        (Some(preset), false) => preset.id().to_string(),
        (None, false) => NaturalPreset::Natural.id().to_string(),
        _ => "custom".to_string(),
    };
    Ok(Choice { params, label })
}

fn preset_ids() -> String {
    NaturalPreset::ALL
        .iter()
        .map(|preset| preset.id())
        .collect::<Vec<_>>()
        .join(", ")
}

fn range(parsed: &Parsed, name: &'static str, min: f32, max: f32) -> Result<Option<f32>> {
    let Some(text) = parsed.value(name) else {
        return Ok(None);
    };
    let value = text
        .parse::<f32>()
        .map_err(|_| CliError::usage(format!("{name} expects a number, got '{text}'")))?;
    if !value.is_finite() || value < min || value > max {
        return Err(CliError::usage(format!(
            "{name} must be in {min}..={max}, got '{text}'"
        )));
    }
    Ok(Some(value))
}

fn parse_format(parsed: &Parsed, default: OutFormat) -> Result<OutFormat> {
    match parsed.value("--format") {
        None => Ok(default),
        Some(text) => OutFormat::parse(text).ok_or_else(|| {
            CliError::usage(format!(
                "unknown --format '{text}'; expected one of: {}",
                OutFormat::ids()
            ))
        }),
    }
}

fn parse_loudness(parsed: &Parsed, default: bool) -> Result<bool> {
    match parsed.value("--loudness") {
        None => Ok(default),
        Some("on") => Ok(true),
        Some("off") => Ok(false),
        Some(other) => Err(CliError::usage(format!(
            "--loudness expects 'on' or 'off', got '{other}'"
        ))),
    }
}

fn parse_presets(parsed: &Parsed) -> Result<Vec<NaturalPreset>> {
    let Some(text) = parsed.value("--presets") else {
        return Ok(NaturalPreset::ALL.to_vec());
    };
    if text == "all" {
        return Ok(NaturalPreset::ALL.to_vec());
    }
    let mut presets = Vec::new();
    for id in text.split(',') {
        let id = id.trim();
        if id.is_empty() {
            continue;
        }
        let preset = NaturalPreset::from_id(id).ok_or_else(|| {
            CliError::usage(format!(
                "unknown preset '{id}'; expected 'all' or a list of: {}",
                preset_ids()
            ))
        })?;
        if !presets.contains(&preset) {
            presets.push(preset);
        }
    }
    if presets.is_empty() {
        return Err(CliError::usage(
            "--presets expects 'all' or a comma-separated preset list",
        ));
    }
    Ok(presets)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputPref {
    Auto,
    F32,
    Weba,
}

fn parse_input_pref(parsed: &Parsed) -> Result<InputPref> {
    match parsed.value("--input") {
        None | Some("auto") => Ok(InputPref::Auto),
        Some("f32") => Ok(InputPref::F32),
        Some("weba") => Ok(InputPref::Weba),
        Some(other) => Err(CliError::usage(format!(
            "--input expects 'auto', 'f32' or 'weba', got '{other}'"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Fixture manifest
// ---------------------------------------------------------------------------

struct ClipEntry {
    name: String,
    voice: String,
    phrase: String,
    language: String,
    sample_count: usize,
    f32_path: Option<PathBuf>,
    weba_path: Option<PathBuf>,
}

impl ClipEntry {
    /// Pick the input path for this clip.
    ///
    /// `auto` prefers the manifest's reference PCM (`files.f32`) because that is
    /// the exact input P-05 measured on and what `metrics.py` compares against;
    /// `weba` exercises the product decoder path. A missing preferred file is a
    /// hard error, not a silent fallback: a batch that quietly rendered a
    /// different source would make the metrics meaningless.
    fn input_path(&self, preference: InputPref, manifest: &Path) -> Result<PathBuf> {
        let chosen = match preference {
            InputPref::F32 => self.f32_path.as_ref(),
            InputPref::Weba => self.weba_path.as_ref(),
            InputPref::Auto => self.f32_path.as_ref().or(self.weba_path.as_ref()),
        };
        chosen.cloned().ok_or_else(|| {
            CliError::failed(format!(
                "{}: the manifest in {} has no {} input file",
                self.name,
                manifest.display(),
                match preference {
                    InputPref::Weba => "weba",
                    _ => "f32 or weba",
                }
            ))
        })
    }
}

fn load_manifest(path: &Path) -> Result<Vec<ClipEntry>> {
    let text = fs::read_to_string(path)
        .map_err(|err| CliError::failed(format!("cannot read {}: {err}", path.display())))?;
    let root =
        json::parse(&text).map_err(|err| CliError::failed(format!("{}: {err}", path.display())))?;
    let clips = root
        .get("clips")
        .and_then(Value::as_array)
        .ok_or_else(|| CliError::failed(format!("{}: no 'clips' array", path.display())))?;
    if clips.is_empty() {
        return Err(CliError::failed(format!(
            "{}: 'clips' is empty; regenerate the fixtures",
            path.display()
        )));
    }

    let base = path.parent().unwrap_or_else(|| Path::new("."));
    // `paths.sources` in the manifest is "relative to the repository's web/".
    let web_root = base.parent().unwrap_or(base).join("web");

    let mut entries = Vec::with_capacity(clips.len());
    for (index, clip) in clips.iter().enumerate() {
        let name = string_field(clip, "name", index)?;
        let voice = string_field(clip, "voice", index)?;
        let phrase = string_field(clip, "phrase", index)?;
        let language = clip
            .get("language")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let sample_count = clip
            .get("sampleCount")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                CliError::failed(format!(
                    "{}: clip {index} ({name}) has no integer sampleCount",
                    path.display()
                ))
            })? as usize;

        let f32_path = clip
            .get("files")
            .and_then(|files| files.get("f32"))
            .and_then(|file| file.get("path"))
            .and_then(Value::as_str)
            .map(|relative| base.join(relative));
        let weba_path = clip
            .get("source")
            .and_then(|source| source.get("path"))
            .and_then(Value::as_str)
            .map(|relative| {
                let under_web = web_root.join(relative);
                if under_web.is_file() {
                    under_web
                } else {
                    base.join(relative)
                }
            });

        entries.push(ClipEntry {
            name,
            voice,
            phrase,
            language,
            sample_count,
            f32_path,
            weba_path,
        });
    }
    Ok(entries)
}

fn string_field(clip: &Value, field: &str, index: usize) -> Result<String> {
    clip.get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| CliError::failed(format!("manifest clip {index} has no string '{field}'")))
}

fn selection_text(voice: Option<&str>, phrase: Option<&str>) -> String {
    match (voice, phrase) {
        (None, None) => "the selection".to_string(),
        (voice, phrase) => format!(
            "voice={} phrase={}",
            voice.unwrap_or("*"),
            phrase.unwrap_or("*")
        ),
    }
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

/// Resolve `--out` / `--out-dir` into one path.
///
/// Exactly one is required. Doing this before loading the input means a bad
/// invocation fails before any expensive work and, more importantly, before
/// anything is written.
fn resolve_output(
    out: Option<PathBuf>,
    out_dir: Option<PathBuf>,
    derived_name: &str,
) -> Result<PathBuf> {
    match (out, out_dir) {
        (Some(path), None) => Ok(path),
        (None, Some(directory)) => Ok(directory.join(derived_name)),
        (None, None) => Err(CliError::usage(
            "one of --out <path> or --out-dir <dir> is required",
        )),
        (Some(_), Some(_)) => Err(CliError::usage(
            "--out and --out-dir are mutually exclusive; give one",
        )),
    }
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|err| {
                CliError::failed(format!(
                    "cannot create directory {}: {err}",
                    parent.display()
                ))
            })?;
        }
    }
    fs::write(path, bytes)
        .map_err(|err| CliError::failed(format!("cannot write {}: {err}", path.display())))
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| "render".to_string())
}

fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

// ---------------------------------------------------------------------------
// Help
// ---------------------------------------------------------------------------

/// `rasp render --help`
pub const RENDER_HELP: &str = "\
Render one clip through the natural chain.

USAGE
  rasp render --in <file> (--out <path> | --out-dir <dir>) [options]

INPUT
  --in <file>            .weba (Opus/WebM, decoded by rasp-opus) or raw 48 kHz
                         mono .f32, or a 48 kHz mono .wav. Required.

OUTPUT
  --out <path>           exact output file. Parent directories are created.
  --out-dir <dir>        output name is <input-stem>.<preset|custom>.<ext>
  --format <id>          wav16 (default), wavf32, f32
                         wav16 is for listening, f32 for metrics: .f32 is what
                         scripts/metrics.py compares byte-equivalently.

PARAMETERS
  --preset <id>          natural | shift-up | shift-down | bright | deep
                         (D12 ids; points are docs/presets.md section 1)
  --pitch <st>           -12..12 semitones, overrides the preset
  --formant <ratio>      0.75..1.33, overrides the preset
  --tilt <db/oct>        -6..6 dB/oct anchored at 1 kHz, overrides the preset
  Overrides win over the preset; with no --preset they start from natural.
  Values outside the documented ranges are rejected, not silently clamped.

BEHAVIOUR
  --trim                 trim detected head/tail silence before rendering.
                         Off by default: the transform preserves length, and a
                         trimmed render is shorter than its source by design
                         (crates/rasp-dsp/src/endpoints.rs). A trimmed render
                         is not sample-aligned with the untrimmed source, so
                         metrics needs --allow-length-delta <removed samples>.
  --loudness on|off      on (default) runs the P-08 policy: whole-clip RMS match
                         to the source, then a -1 dBTP true-peak limiter.
                         off is the raw transform, for measurement only.
  --json                 machine summary on stdout, human summary on stderr.

EXAMPLES
  # a listening file, D12 preset
  rasp render --in web/audio/af_heart/greet.weba \\
              --preset deep --out /tmp/deep.wav

  # explicit knobs, no preset; f32 for the metrics harness
  rasp render --in clip.f32 --pitch 4 --formant 1.10 --format f32 --out /tmp/x.f32

  # preset plus one override: deep, but only -2 st
  rasp render --in clip.weba --preset deep --pitch -2 --out /tmp/deep2.wav

  # trimmed render: the output is shorter than the source
  rasp render --in clip.weba --trim --out /tmp/trimmed.wav

  # machine-readable, for a script
  rasp render --in clip.weba --preset bright --out /tmp/b.wav --json | jq .

  # a single render's numbers, including true peak and level delta
  scripts/metrics.py --ref fixtures/f32/af_heart__friends-now.f32 \\
      --test /tmp/x.f32 --pitch-st 4 --formant-ratio 1.10 --check
";

/// `rasp batch --help`
pub const BATCH_HELP: &str = "\
Render a set of fixtures, plus a manifest.tsv for scripts/metrics.py.

USAGE
  rasp batch (--manifest <file> | --voice <id> --phrase <id> | --all) [options]

SELECTION
  --manifest <file>      fixture manifest (default fixtures/manifest.json).
                         With no filter, every clip in it is rendered.
  --voice <id>           keep clips whose voice matches, e.g. af_heart
  --phrase <id>          keep clips whose phrase matches, e.g. greet
  --all                  every clip in the manifest (the default with
                         --manifest; refuse to guess without a selection)

OPTIONS
  --presets <list>       all (default) or a comma list:
                         natural,shift-up,shift-down,bright,deep
  --out-dir <dir>        output directory (default rasp-renders). Created.
  --format <id>          f32 (default), wav16, wavf32
                         f32 is the default because batch exists to feed
                         metrics.py; use wav16 for a listening pack.
  --input <auto|f32|weba>
                         auto (default) uses the manifest's files.f32 (the
                         reference PCM P-05 measured on and metrics compares
                         against). weba renders through the product decoder.
  --trim                 trim endpoints before each render (output is shorter;
                         metrics then needs --allow-length-delta)
  --loudness on|off      P-08 policy (default on)
  --json                 one object with every render on stdout

OUTPUT
  <name>.<preset>.<ext>  one file per fixture x preset
  manifest.tsv           fixture, step, cents, ratio, preset, pitch_st,
                         formant, tilt_db_per_oct, file, samples, peak_dbfs,
                         true_peak_dbtp, level_delta_db, language
                         `ratio` is the PITCH ratio (scripts/metrics.py's
                         meaning); the formant ratio is the `formant` column.
                         Timings are not written here: the manifest must be
                         reproducible.

EXAMPLES
  # the P-06/P-05 regression render set, then score it
  rasp batch --manifest fixtures/manifest.json --presets all \\
             --out-dir /tmp/rasp-p06-renders
  scripts/.venv/bin/python scripts/metrics.py \\
      --renders /tmp/rasp-p06-renders \\
      --manifest /tmp/rasp-p06-renders/manifest.tsv --check

  # one voice, one phrase, the two size presets
  rasp batch --voice am_onyx --phrase got-time \\
             --presets shift-up,shift-down --out-dir /tmp/onyx

  # a 16-bit listening pack for every fixture and preset
  rasp batch --all --presets all --format wav16 --out-dir /tmp/listen

  # the product decoder path instead of the fixture PCM
  rasp batch --all --input weba --out-dir /tmp/from-weba

Note: batch renders sequentially in manifest order, so manifests and JSON are
byte-identical between runs. Parallelism was not worth nondeterministic output.
";

/// `rasp decode --help`
pub const DECODE_HELP: &str = "\
Decode a clip with no DSP: the reference path.

USAGE
  rasp decode --in <file> (--out <path> | --out-dir <dir>) [--format <id>]

  --in <file>            .weba, raw .f32, or .wav (48 kHz mono)
  --out <path>           exact output file
  --out-dir <dir>        output name is <input-stem>.<ext>
  --format <id>          f32 (default), wav16, wavf32
  --json                 machine summary on stdout, human summary on stderr

The samples are written unchanged: this is the reference against which any
render can be compared (and the D-02 framing behaviour, not a transform).

EXAMPLES
  rasp decode --in web/audio/af_heart/greet.weba --out /tmp/greet.f32
  rasp decode --in clip.weba --out-dir /tmp/ref --format wavf32
  rasp decode --in clip.weba --out /tmp/greet.wav --json | jq .samples

  # D-02 framing check: decoded length must equal the manifest's sampleCount
  rasp info --in web/audio/af_heart/greet.weba --json | jq .frames
";

/// `rasp info --help`
pub const INFO_HELP: &str = "\
Report what is in a clip, without changing it.

USAGE
  rasp info --in <file> [--json]

  --in <file>            .weba, raw .f32, or .wav (48 kHz mono)
  --json                 one object on stdout

Reports frames, rate, channels, duration, sample peak (dBFS), 4x true peak
(dBTP), RMS, and the endpoint detector's head/tail silence with the samples a
trim would keep. The endpoint rule is F-06's -45 dBFS / 10 ms / 20 ms detector
with the P-04 safety margins (crates/rasp-dsp/src/endpoints.rs).

EXAMPLES
  rasp info --in web/audio/af_heart/greet.weba
  rasp info --in /tmp/render.f32 --json | jq '{frames, peakDbfs, truePeakDbtp}'
  # confirm a trimmed render is short by the reported removed samples
  rasp info --in clip.weba --json
";

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> Parsed {
        let tokens: Vec<String> = text.split_whitespace().map(str::to_string).collect();
        args::parse(Spec::new(RENDER_VALUES, RENDER_FLAGS), &tokens).expect("parses")
    }

    #[test]
    fn preset_is_filled_and_overrides_win() {
        let choice = resolve_params(&parsed("--in x --out y --preset deep")).expect("resolves");
        assert_eq!(choice.label, "deep");
        assert_eq!(choice.params.pitch_st, -4.0);
        assert_eq!(choice.params.formant_ratio, 0.85);

        let choice =
            resolve_params(&parsed("--in x --out y --preset deep --pitch 2")).expect("resolves");
        assert_eq!(choice.label, "custom");
        assert_eq!(choice.params.pitch_st, 2.0);
        assert_eq!(choice.params.formant_ratio, 0.85);
        assert_eq!(choice.params.tilt_db_per_oct, 0.0);
    }

    #[test]
    fn defaults_are_natural_and_bad_values_are_rejected() {
        let choice = resolve_params(&parsed("--in x --out y")).expect("resolves");
        assert_eq!(choice.label, "natural");
        assert!(choice.params.is_natural());

        // Parameter/option values, all on the render command.
        for text in [
            "--in x --out y --preset child",
            "--in x --out y --pitch 20",
            "--in x --out y --formant 2",
            "--in x --out y --tilt nan",
            "--in x --out y --format mp3",
            "--in x --out y --loudness maybe",
        ] {
            let parsed = parsed(text);
            let outcome = if text.contains("--format") {
                parse_format(&parsed, OutFormat::Wav16).map(|_| ())
            } else if text.contains("--loudness") {
                parse_loudness(&parsed, true).map(|_| ())
            } else {
                resolve_params(&parsed).map(|_| ())
            };
            assert!(outcome.is_err(), "{text} should be rejected");
        }

        // The batch preset list has its own spec.
        let tokens: Vec<String> = ["--manifest", "m.json", "--presets", "shift-up,bogus"]
            .iter()
            .map(|token| token.to_string())
            .collect();
        let parsed = args::parse(Spec::new(BATCH_VALUES, BATCH_FLAGS), &tokens).expect("parses");
        assert!(parse_presets(&parsed).is_err());
    }

    #[test]
    fn manifest_rows_carry_the_metrics_columns() {
        let record = RenderRecord {
            path: PathBuf::from("/tmp/a__b.shift-up.f32"),
            bytes: 1,
            format: OutFormat::RawF32,
            fixture: Some("a__b".to_string()),
            language: Some("en".to_string()),
            label: "shift-up".to_string(),
            params: NaturalPreset::ShiftUp.params(),
            loudness: true,
            trim: false,
            samples_in: 10,
            source_samples: 10,
            samples_out: 10,
            sample_rate: 48_000,
            peak_dbfs: -3.0,
            true_peak_dbtp: -2.9,
            source_rms_dbfs: -16.0,
            output_rms_dbfs: -16.1,
            level_delta_db: -0.1,
            limiter_min_gain_db: 0.0,
            decode_ms: 0.1,
            render_ms: 1.0,
            write_ms: 0.1,
        };
        let row = manifest_row(&record);
        let fields: Vec<&str> = row.trim_end().split('\t').collect();
        assert_eq!(fields[0], "a__b");
        assert_eq!(fields[1], "+7");
        assert_eq!(fields[2], "700.000000");
        assert_eq!(fields[4], "shift-up");
        assert_eq!(fields[8], "a__b.shift-up.f32");
        assert_eq!(fields.len(), 14);
    }

    #[test]
    fn preset_lists_are_ordered_and_deduplicated() {
        let parsed = args::parse(
            Spec::new(BATCH_VALUES, BATCH_FLAGS),
            &[
                "--manifest".to_string(),
                "m.json".to_string(),
                "--presets".to_string(),
                "deep,natural,deep".to_string(),
            ],
        )
        .expect("parses");
        let presets = parse_presets(&parsed).expect("parses presets");
        assert_eq!(presets, vec![NaturalPreset::Deep, NaturalPreset::Natural]);
    }
}
