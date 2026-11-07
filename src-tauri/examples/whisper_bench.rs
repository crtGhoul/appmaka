//! whisper-cli benchmark harness (dev-only; not part of the app).
//!
//! Runs the pinned whisper.cpp binary the same way dictation does
//! (`-m <model> -f <wav> -l en -nt -t 4`) and reports wall time plus
//! the engine's own total from its `whisper_print_timings` output, as
//! a real-time factor (engine seconds per audio second — below 1.0 is
//! faster than real time). Also surfaces the backend banner lines from
//! stderr (ggml prints which backend/device it initialized), so a CPU
//! run and a CUDA run are distinguishable from the output alone.
//!
//! Std-only: this crate has no lib target for examples to link, and a
//! bench harness needs nothing beyond process spawning and parsing.
//!
//! Usage: cargo run --example whisper_bench -- \
//!   --bin <whisper-cli> --model <ggml.bin> --audio <wav> [--runs N]

use std::io::Read;
use std::process::{exit, Command};
use std::time::Instant;

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone())
}

fn fail(msg: &str) -> ! {
    eprintln!("whisper_bench: {msg}");
    exit(1);
}

/// Minimal WAV reader: audio duration from the header (data chunk
/// size / byte rate), walking chunks so extra metadata chunks are fine.
fn wav_duration_secs(path: &str) -> f64 {
    let mut f = std::fs::File::open(path).unwrap_or_else(|e| fail(&format!("open {path}: {e}")));
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes)
        .unwrap_or_else(|e| fail(&format!("read {path}: {e}")));
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        fail(&format!("{path} is not a WAV file"));
    }
    let mut byte_rate: u64 = 0;
    let mut data_len: u64 = 0;
    let mut pos = 12usize;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = pos + 8;
        match id {
            b"fmt " if body + 12 <= bytes.len() => {
                // fmt layout: format(2) channels(2) sample_rate(4)
                // byte_rate(4) — byte_rate sits at offset 8.
                byte_rate = u32::from_le_bytes(bytes[body + 8..body + 12].try_into().unwrap()) as u64;
            }
            b"data" => data_len = size as u64,
            _ => {}
        }
        // Chunks are word-aligned.
        pos = body + size + (size % 2);
    }
    if byte_rate == 0 || data_len == 0 {
        fail(&format!("{path}: missing fmt/data chunk"));
    }
    data_len as f64 / byte_rate as f64
}

/// The ggml backend banner: the lines where ggml says what it
/// initialized (e.g. a CUDA device line on GPU builds, the CPU feature
/// line / system_info on CPU builds). Verbatim, capped at 4 lines.
fn backend_banner(stderr: &str) -> Vec<String> {
    let mut lines: Vec<String> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| {
            let low = l.to_lowercase();
            (l.contains("ggml") || l.starts_with("system_info"))
                && (low.contains("cuda")
                    || low.contains("metal")
                    || low.contains("vulkan")
                    || low.contains("cpu")
                    || low.contains("backend")
                    || low.contains("device")
                    || l.starts_with("system_info"))
        })
        .map(str::to_string)
        .collect();
    lines.dedup();
    lines.truncate(4);
    lines
}

/// whisper-cli prints "whisper_print_timings:    total time =   545.32 ms".
fn engine_total_secs(stderr: &str) -> Option<f64> {
    for line in stderr.lines() {
        if line.contains("whisper_print_timings") && line.contains("total time") {
            let after_eq = line.split('=').nth(1)?.trim();
            let number = after_eq.trim_end_matches("ms").trim();
            return number.parse::<f64>().ok().map(|ms| ms / 1000.0);
        }
    }
    None
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    values[values.len() / 2]
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let bin = arg_value(&args, "--bin").unwrap_or_else(|| fail("--bin <whisper-cli> is required"));
    let model = arg_value(&args, "--model").unwrap_or_else(|| fail("--model <ggml.bin> is required"));
    let audio = arg_value(&args, "--audio").unwrap_or_else(|| fail("--audio <wav> is required"));
    let runs: usize = arg_value(&args, "--runs")
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);

    let audio_secs = wav_duration_secs(&audio);
    println!(
        "audio={audio} duration={audio_secs:.2}s model={model} runs={runs}"
    );

    let mut engine_totals: Vec<f64> = Vec::new();
    let mut banners: Vec<String> = Vec::new();
    for run in 1..=runs {
        let started = Instant::now();
        // Same invocation dictation uses (run_whisper in dictation.rs).
        let out = Command::new(&bin)
            .arg("-m")
            .arg(&model)
            .arg("-f")
            .arg(&audio)
            .arg("-l")
            .arg("en")
            .arg("-nt")
            .arg("-t")
            .arg("4")
            .output()
            .unwrap_or_else(|e| fail(&format!("spawn {bin}: {e}")));
        let wall_ms = started.elapsed().as_millis();
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !out.status.success() {
            fail(&format!("whisper-cli exited {}: {}", out.status, stderr.trim()));
        }
        if run == 1 {
            banners = backend_banner(&stderr);
        }
        match engine_total_secs(&stderr) {
            Some(total) => {
                engine_totals.push(total);
                println!(
                    "run {run}: wall={wall_ms} ms engine_total={total:.3} s rtf={:.4}",
                    total / audio_secs
                );
            }
            None => println!("run {run}: wall={wall_ms} ms engine_total=? (no timings line)"),
        }
    }
    for line in &banners {
        println!("banner: {line}");
    }
    if !engine_totals.is_empty() {
        let med = median(engine_totals);
        println!(
            "summary: model={} audio={:.2}s runs={} median_engine={:.3}s median_rtf={:.4}",
            model.rsplit(['/', '\\']).next().unwrap_or(&model),
            audio_secs,
            runs,
            med,
            med / audio_secs
        );
    }
}
