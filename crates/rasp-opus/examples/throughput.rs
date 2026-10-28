//! Measure native decode throughput in milliseconds per second of audio.
//!
//! Build and run with:
//!
//! ```text
//! cargo run --release -p rasp-opus --example throughput -- \
//!     --iters 100 web/audio/af_heart/be-friends.weba web/audio/im_nicola/see-you.weba
//! ```
//!
//! The file is read once; the timed loop is [`decode_weba`] over those bytes, so
//! the number is decode cost and not disk I/O. For each file this decodes once
//! as a warm-up, then times `--iters` iterations and reports the fastest and
//! mean wall time, plus `ms/s` (lower is better). `ms/s` is milliseconds spent
//! decoding one second of audio, so `1.0` means 1000× realtime.

use std::time::Instant;

use rasp_opus::{decode_weba, decode_weba_file};

fn main() {
    let mut iters = 100usize;
    let mut paths: Vec<String> = Vec::new();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--iters" {
            match args.next().and_then(|value| value.parse::<usize>().ok()) {
                Some(value) if value > 0 => iters = value,
                _ => {
                    eprintln!("--iters needs a positive integer");
                    std::process::exit(2);
                }
            }
        } else {
            paths.push(arg);
        }
    }

    if paths.is_empty() {
        eprintln!("usage: throughput [--iters N] <clip.weba>...");
        std::process::exit(2);
    }

    for path in paths {
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) => {
                eprintln!("{path}: {err}");
                std::process::exit(1);
            }
        };

        let clip = match decode_weba(&bytes) {
            Ok(clip) => clip,
            Err(err) => {
                eprintln!("{path}: {err}");
                std::process::exit(1);
            }
        };
        let seconds = clip.duration_secs();

        // Sanity check that the file-backed entry point agrees with the
        // in-memory one; it is not part of the timed loop.
        match decode_weba_file(&path) {
            Ok(file_clip) if file_clip.samples.len() == clip.samples.len() => {}
            Ok(_) => {
                eprintln!("{path}: file and in-memory decode disagree");
                std::process::exit(1);
            }
            Err(err) => {
                eprintln!("{path}: {err}");
                std::process::exit(1);
            }
        }

        let mut best_ms = f64::INFINITY;
        let mut total_ms = 0.0f64;

        for _ in 0..iters {
            let start = Instant::now();
            let decoded = decode_weba(&bytes);
            let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;

            match decoded {
                Ok(decoded) => {
                    std::hint::black_box(decoded.samples.len());
                }
                Err(err) => {
                    eprintln!("{path}: {err}");
                    std::process::exit(1);
                }
            }

            best_ms = best_ms.min(elapsed_ms);
            total_ms += elapsed_ms;
        }

        println!(
            "{path}: frames={} secs={seconds:.3} iters={iters} \
             best_ms={best_ms:.3} mean_ms={:.3} best_ms_per_s={:.3} mean_ms_per_s={:.3}",
            clip.frame_count(),
            total_ms / iters as f64,
            best_ms / seconds,
            total_ms / iters as f64 / seconds,
        );
    }
}
