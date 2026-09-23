// SPDX-License-Identifier: Apache-2.0
//! One-shot index and encode profiling on caller-supplied corpus files.
//!
//! Run the compiled example under `/usr/bin/time -v` to measure peak RSS.

use std::{env, fs, hint::black_box, process, time::Instant};

use heddle_format::delta::DeltaEncoder;

fn main() {
    let mut args = env::args().skip(1);
    let (Some(mode), Some(base_path), Some(target_path), None) =
        (args.next(), args.next(), args.next(), args.next())
    else {
        eprintln!("usage: delta_profile <index|encode> <base> <target>");
        process::exit(2);
    };

    let base = fs::read(&base_path).unwrap_or_else(|error| {
        eprintln!("{base_path}: {error}");
        process::exit(1);
    });
    let target = fs::read(&target_path).unwrap_or_else(|error| {
        eprintln!("{target_path}: {error}");
        process::exit(1);
    });

    let started = Instant::now();
    let index = DeltaEncoder::build_index(black_box(&base));
    let index_seconds = started.elapsed().as_secs_f64();
    black_box(&index);

    if mode == "index" {
        println!("index_seconds={index_seconds:.6} base_bytes={}", base.len());
        return;
    }
    if mode != "encode" {
        eprintln!("unknown mode: {mode}");
        process::exit(2);
    }

    let started = Instant::now();
    let delta = DeltaEncoder::encode_with_index(&index, &base, &target);
    let encode_seconds = started.elapsed().as_secs_f64();
    println!(
        "index_seconds={index_seconds:.6} encode_seconds={encode_seconds:.6} base_bytes={} target_bytes={} delta_bytes={} ratio={:.8}",
        base.len(),
        target.len(),
        delta.len(),
        delta.len() as f64 / target.len() as f64
    );
    black_box(delta);
}
