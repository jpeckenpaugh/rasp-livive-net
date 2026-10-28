//! `rasp` — the offline command-line front end for the RASP voice transform.
//!
//! PLAN P-06. The CLI is the enabler for listening tests (HR-2), for Q-03's
//! regression renders, and for `scripts/metrics.py --renders`. Its output
//! contract is therefore treated as an API: see `README.md` in this crate.
//!
//! Supported commands:
//!
//! ```text
//! rasp render  one clip through the natural chain (preset + overrides)
//! rasp batch   fixtures -> renders + manifest.tsv for scripts/metrics.py
//! rasp decode  .weba -> .f32/.wav with no DSP (the reference path)
//! rasp info    frames, rate, channels, duration, peak, true peak, endpoints
//! ```
//!
//! No external dependencies: `rasp-dsp` (the product chain, including P-08's
//! loudness stage) and `rasp-opus` (the D-01 decoder) are in-workspace crates.
//! Argument parsing and the tiny JSON reader are hand-rolled.
//!
//! Failure contract: every user-facing failure is one line on stderr and a
//! non-zero exit; there is no `unwrap`/`expect` on anything derived from input.

mod args;
mod audio;
mod commands;
mod json;
mod report;

use std::process::ExitCode;

/// The CLI's error type: usage problems exit 2, runtime failures exit 1.
///
/// Both carry a single line, already free of backtraces and type names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    /// The command line is wrong (unknown flag, missing value, bad range).
    Usage(String),
    /// The command line is fine but the work failed (unreadable file, …).
    Failed(String),
}

impl CliError {
    /// A usage error, prefixed with the subcommand name by the caller when it
    /// matters. This constructor keeps the call sites readable.
    pub fn usage(message: impl Into<String>) -> Self {
        CliError::Usage(message.into())
    }

    /// A runtime failure.
    pub fn failed(message: impl Into<String>) -> Self {
        CliError::Failed(message.into())
    }

    /// The diagnostic without its one-line prefix.
    pub fn message(&self) -> &str {
        match self {
            CliError::Usage(message) | CliError::Failed(message) => message,
        }
    }
}

/// Audio I/O reports its failures as `String`; they are runtime failures.
impl From<String> for CliError {
    fn from(message: String) -> Self {
        CliError::Failed(message)
    }
}

/// The result type every command returns.
pub type Result<T> = std::result::Result<T, CliError>;

fn main() -> ExitCode {
    let tokens: Vec<String> = std::env::args().skip(1).collect();

    let outcome = match tokens.first().map(String::as_str) {
        None => {
            println!("{HELP}");
            Ok(())
        }
        Some("-h") | Some("--help") | Some("help") => {
            println!("{HELP}");
            Ok(())
        }
        Some("-V") | Some("--version") => {
            println!("rasp {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("render") => commands::render(&tokens[1..]),
        Some("batch") => commands::batch(&tokens[1..]),
        Some("decode") => commands::decode(&tokens[1..]),
        Some("info") => commands::info(&tokens[1..]),
        Some(other) if other.starts_with('-') => Err(CliError::usage(format!(
            "unknown option '{other}' before a command (see --help)"
        ))),
        Some(other) => Err(CliError::usage(format!(
            "unknown command '{other}'; expected render, batch, decode or info"
        ))),
    };

    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(CliError::Usage(message)) => {
            eprintln!("rasp: {message}");
            eprintln!("Try 'rasp --help' for usage.");
            ExitCode::from(2)
        }
        Err(CliError::Failed(message)) => {
            eprintln!("rasp: {message}");
            ExitCode::from(1)
        }
    }
}

/// `rasp --help`
pub const HELP: &str = "\
rasp — offline renderer for the RASP voice transform (PLAN P-06)

USAGE
  rasp <command> [options]
  rasp <command> --help      options and examples for one command

COMMANDS
  render   Render one clip through the natural chain: preset and/or explicit
           pitch, formant and tilt. Writes wav16 (listening) or f32 (metrics).
  batch    Render fixtures from fixtures/manifest.json (or a voice/phrase
           filter) and write manifest.tsv for scripts/metrics.py --renders.
  decode   Decode .weba to .f32/.wav with no DSP — the reference path.
  info     Report frames, rate, channels, duration, sample peak, 4x true peak
           and the endpoint head/tail silence.

  --version                 print the version
  --help                    this text

WHAT A RENDER IS
  input -> VoiceTransformer (pitch / formant / tilt) -> LoudnessStage
        -> sample peak / 4x true peak -> file

  The chain is the product chain from crates/rasp-dsp: the same code the
  AudioWorklet will run (D2/D3/D19). The D12 presets are the operating points in
  docs/presets.md section 1. The P-08 loudness policy (whole-clip RMS match, then
  a -1 dBTP true-peak limiter) is on by default and can be switched off for
  measurement.

  Output is deterministic: the same input and arguments produce byte-identical
  files. Timings are reported but never written into a render or a manifest.

QUICK START
  # one listening file from a corpus clip
  rasp render --in web/audio/af_heart/greet.weba --preset deep --out /tmp/deep.wav

  # explicit knobs (D1: pitch in semitones, formant as a ratio)
  rasp render --in clip.f32 --pitch 4 --formant 1.10 --format f32 --out /tmp/x.f32

  # the whole fixture x preset regression set for metrics
  rasp batch --manifest fixtures/manifest.json --presets all --out-dir /tmp/rasp
  scripts/.venv/bin/python scripts/metrics.py \\
      --renders /tmp/rasp --manifest /tmp/rasp/manifest.tsv --check

  # the reference decode, no DSP
  rasp decode --in web/audio/af_heart/greet.weba --out /tmp/greet.f32

  # what is in a file, including what a trim would remove
  rasp info --in web/audio/af_heart/greet.weba

  # a 16-bit listening pack for HR-2 (all 12 fixtures x all 5 presets)
  rasp batch --all --presets all --format wav16 --out-dir /tmp/hr2-pack

INPUT
  48 kHz mono .weba (Opus/WebM, decoded by rasp-opus), raw little-endian .f32,
  or .wav (PCM 16/32-bit or float 32-bit). Other rates or channel counts are
  rejected rather than silently resampled; the corpus is 48 kHz mono (PLAN K4).

EXIT CODES
  0  success (including `--json` output)
  1  the work failed (unreadable input, render error, unwritable output)
  2  the command line is wrong (unknown flag, missing value, out-of-range knob)
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_keep_one_line_and_the_right_kind() {
        let usage = CliError::usage("bad flag");
        assert_eq!(usage.message(), "bad flag");
        assert!(matches!(usage, CliError::Usage(_)));
        let failed = CliError::failed("no file");
        assert!(matches!(failed, CliError::Failed(_)));
    }

    #[test]
    fn help_text_documents_every_command_and_example() {
        for needle in [
            "render",
            "batch",
            "decode",
            "info",
            "QUICK START",
            "metrics.py",
        ] {
            assert!(HELP.contains(needle), "help is missing {needle}");
        }
        for help in [
            commands::RENDER_HELP,
            commands::BATCH_HELP,
            commands::DECODE_HELP,
            commands::INFO_HELP,
        ] {
            assert!(help.contains("EXAMPLES"), "a command help has no examples");
        }
    }
}
