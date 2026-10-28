use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: decode <clip.weba> [out.f32] [--iters N]");
        std::process::exit(2);
    }
    let mut out: Option<&str> = None;
    let mut iters = 20usize;
    let mut i = 2;
    while i < args.len() {
        if args[i] == "--iters" {
            iters = args[i + 1].parse().expect("iters");
            i += 2;
        } else {
            out = Some(&args[i]);
            i += 1;
        }
    }
    let bytes = std::fs::read(&args[1]).expect("read clip");
    // warm-up
    let samples = rasp_decode_spike::decode(&bytes);
    let mut best = f64::MAX;
    let mut total = 0.0f64;
    for _ in 0..iters {
        let t = Instant::now();
        std::hint::black_box(rasp_decode_spike::decode(&bytes));
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        best = best.min(ms);
        total += ms;
    }
    let n = samples.len() as f64;
    let secs = n / 48_000.0;
    println!(
        "backend={} file={} samples={} secs={:.6} packets_n/a min_ms={:.3} mean_ms={:.3} ms_per_s={:.3}",
        rasp_decode_spike::backend_name(),
        args[1],
        samples.len(),
        secs,
        best,
        total / iters as f64,
        best / secs
    );
    if let Some(path) = out {
        let mut buf = Vec::with_capacity(samples.len() * 4);
        for s in &samples {
            buf.extend_from_slice(&s.to_le_bytes());
        }
        std::fs::write(path, buf).expect("write pcm");
    }
}
