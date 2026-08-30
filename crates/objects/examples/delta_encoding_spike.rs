// SPDX-License-Identifier: Apache-2.0
//! Measurement harness for heddle#1610. This is not a candidate pack format.
//!
//! Run the complete matrix with:
//!
//! ```text
//! cargo run -p heddle-objects --example delta_encoding_spike \
//!   --features bench,zstd --release
//! ```
//!
//! Set `HEDDLE_DELTA_SPIKE_QUICK=1` for a compile/smoke run and
//! `HEDDLE_DELTA_SPIKE_SAMPLES=<n>` to change the default three timing samples.

use std::{
    hint::black_box,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use heddle_format::delta::{DeltaDecoder, DeltaEncoder};
use heddle_object_model::{
    compact::{decode_blob_frame, encode_blob_frame},
    object::ContentHash,
};
use heddle_pack::store::pack::{compress_compact_frame, decompress_pack_payload, has_zstd_magic};

const BLOB_SIZE: usize = 256 * 1024;
const DELTA_MAGIC: &[u8; 4] = b"D161";
const CHECKSUM_LEN: usize = 32;

#[derive(Clone, Copy, Debug)]
enum Encoding {
    CurrentRaw,
    CurrentZstd,
    DeltaRaw,
    DeltaZstd,
}

impl Encoding {
    const ALL: [Self; 4] = [
        Self::CurrentRaw,
        Self::CurrentZstd,
        Self::DeltaRaw,
        Self::DeltaZstd,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::CurrentRaw => "current-raw",
            Self::CurrentZstd => "current-zstd19-ldm",
            Self::DeltaRaw => "delta-raw",
            Self::DeltaZstd => "delta-zstd19-ldm",
        }
    }

    fn is_delta(self) -> bool {
        matches!(self, Self::DeltaRaw | Self::DeltaZstd)
    }

    fn uses_zstd(self) -> bool {
        matches!(self, Self::CurrentZstd | Self::DeltaZstd)
    }
}

struct Corpus {
    name: String,
    blobs: Vec<Vec<u8>>,
}

struct Prepared {
    stored: Vec<u8>,
    logical_len: usize,
    compressed: bool,
}

struct DeltaLayout<'a> {
    target_lengths: Vec<usize>,
    sections: Vec<&'a [u8]>,
}

struct RssMeasurement<T> {
    value: T,
    baseline_bytes: u64,
    peak_bytes: u64,
}

fn main() -> Result<()> {
    let quick = std::env::var_os("HEDDLE_DELTA_SPIKE_QUICK").is_some();
    let samples = std::env::var("HEDDLE_DELTA_SPIKE_SAMPLES")
        .ok()
        .map(|value| value.parse::<usize>())
        .transpose()
        .context("HEDDLE_DELTA_SPIKE_SAMPLES must be a positive integer")?
        .unwrap_or(3);
    if samples == 0 {
        bail!("HEDDLE_DELTA_SPIKE_SAMPLES must be greater than zero");
    }

    print_metadata(samples, quick)?;
    println!(
        "corpus,objects,raw_bytes,object_zstd_bytes,encoding,stored_mode,encoded_bytes,ratio_raw,ratio_object_zstd,encode_ms,decode_all_ms,random_id_ms,indexed_ms,encode_peak_rss_mib,encode_delta_rss_mib,read_peak_rss_mib,read_delta_rss_mib"
    );

    if quick {
        measure_corpus(
            &synthetic_corpus(EditPattern::Localized { bytes: 64 }, 8),
            samples,
        )?;
        return Ok(());
    }
    for revisions in [8, 32, 128] {
        measure_corpus(
            &synthetic_corpus(EditPattern::Localized { bytes: 64 }, revisions),
            samples,
        )?;
        measure_corpus(
            &synthetic_corpus(
                EditPattern::Scattered {
                    edits: 8,
                    bytes_each: 8,
                },
                revisions,
            ),
            samples,
        )?;
        measure_corpus(
            &synthetic_corpus(EditPattern::Localized { bytes: 4096 }, revisions),
            samples,
        )?;
        measure_corpus(&random_corpus(revisions), samples)?;
    }
    measure_corpus(&real_cargo_lock_corpus(32)?, samples)
}

fn print_metadata(samples: usize, quick: bool) -> Result<()> {
    let head = command_stdout("git", &["rev-parse", "HEAD"])?;
    let rustc = command_stdout("rustc", &["--version"])?;
    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("model name\t: "))
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "unknown".to_owned());
    println!("# spike=heddle-1610");
    println!("# git_head={}", head.trim());
    println!("# rustc={}", rustc.trim());
    println!("# cpu={cpu}");
    println!("# timing=median samples={samples} release=true quick={quick}");
    println!("# random_target=newest/deepest content-id lookup outside timed region");
    println!("# memory=peak sampled VmRSS; delta is above prepared-input baseline");
    Ok(())
}

#[derive(Clone, Copy)]
enum EditPattern {
    Localized { bytes: usize },
    Scattered { edits: usize, bytes_each: usize },
}

fn synthetic_corpus(pattern: EditPattern, revisions: usize) -> Corpus {
    let mut blobs = Vec::with_capacity(revisions + 1);
    let mut current = random_bytes(BLOB_SIZE, 0x1610_5eed_cafe_babe);
    blobs.push(current.clone());

    for revision in 1..=revisions {
        let mut state = 0xd1ff_0000_0000_0000u64 ^ revision as u64;
        match pattern {
            EditPattern::Localized { bytes } => {
                let span = current.len().saturating_sub(bytes).max(1);
                let start = revision.wrapping_mul(7_919) % span;
                mutate_range(&mut current, start, bytes, &mut state);
            }
            EditPattern::Scattered { edits, bytes_each } => {
                for edit in 0..edits {
                    state = next_random(state ^ edit as u64);
                    let span = current.len().saturating_sub(bytes_each).max(1);
                    let start = state as usize % span;
                    mutate_range(&mut current, start, bytes_each, &mut state);
                }
            }
        }
        blobs.push(current.clone());
    }

    let name = match pattern {
        EditPattern::Localized { bytes } => format!("synthetic-local{bytes}-r{revisions}"),
        EditPattern::Scattered { edits, bytes_each } => {
            format!("synthetic-scatter{}x{}-r{revisions}", edits, bytes_each)
        }
    };
    Corpus { name, blobs }
}

fn random_corpus(revisions: usize) -> Corpus {
    let blobs = (0..=revisions)
        .map(|index| random_bytes(BLOB_SIZE, 0xa11c_e000_0000_0000 ^ index as u64))
        .collect();
    Corpus {
        name: format!("independent-random-r{revisions}"),
        blobs,
    }
}

fn real_cargo_lock_corpus(versions: usize) -> Result<Corpus> {
    let revisions = command_stdout(
        "git",
        &[
            "log",
            &format!("--max-count={versions}"),
            "--format=%H",
            "HEAD",
            "--",
            "Cargo.lock",
        ],
    )?;
    let mut commits = revisions.lines().collect::<Vec<_>>();
    commits.reverse();
    if commits.len() < versions {
        bail!(
            "real Cargo.lock corpus requested {versions} versions but found {}",
            commits.len()
        );
    }
    let mut blobs = Vec::with_capacity(commits.len());
    for commit in commits {
        let spec = format!("{commit}:Cargo.lock");
        let output = Command::new("git")
            .args(["show", &spec])
            .output()
            .with_context(|| format!("failed to run git show {spec}"))?;
        if !output.status.success() {
            bail!(
                "git show {spec} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        blobs.push(output.stdout);
    }
    Ok(Corpus {
        name: format!("real-Cargo.lock-v{versions}"),
        blobs,
    })
}

fn command_stdout(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("failed to run {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout).with_context(|| format!("{program} emitted non-UTF-8"))
}

fn random_bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed;
    let mut bytes = Vec::with_capacity(len);
    while bytes.len() < len {
        state = next_random(state);
        bytes.extend_from_slice(&state.to_le_bytes());
    }
    bytes.truncate(len);
    bytes
}

fn next_random(mut state: u64) -> u64 {
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    state
}

fn mutate_range(blob: &mut [u8], start: usize, len: usize, state: &mut u64) {
    let end = start.saturating_add(len).min(blob.len());
    for byte in &mut blob[start..end] {
        *state = next_random(*state);
        *byte = (*state >> 24) as u8;
    }
}

fn measure_corpus(corpus: &Corpus, samples: usize) -> Result<()> {
    let raw_bytes = corpus.blobs.iter().map(Vec::len).sum::<usize>();
    let object_zstd_bytes = corpus
        .blobs
        .iter()
        .map(|blob| compress_compact_frame(blob).map(|stored| stored.len()))
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .sum::<usize>();
    let target = corpus
        .blobs
        .last()
        .ok_or_else(|| anyhow!("corpus {} is empty", corpus.name))?;
    let target_id = ContentHash::compute_typed("blob", target);

    for encoding in Encoding::ALL {
        let encode_ms = median_ms(samples, || encode(corpus, encoding))?;
        let prepared = encode(corpus, encoding)?;
        verify_prepared(corpus, encoding, &prepared, target_id)?;
        let decode_all_ms = median_ms(samples, || {
            decode_all_metric(encoding, &prepared).map(black_box)
        })?;
        let random_samples = samples.saturating_mul(3).max(5);
        let random_id_ms = median_ms(random_samples, || {
            read_by_content_id(encoding, &prepared, target_id).map(black_box)
        })?;
        let indexed_ms = median_ms(random_samples, || {
            read_indexed(encoding, &prepared, corpus.blobs.len() - 1).map(black_box)
        })?;

        trim_allocator();
        let encode_memory = measure_rss(|| encode(corpus, encoding))?;
        black_box(&encode_memory.value);
        drop(encode_memory.value);
        trim_allocator();
        let read_memory = measure_rss(|| read_by_content_id(encoding, &prepared, target_id))?;
        if read_memory.value.as_slice() != target.as_slice() {
            bail!("memory read validation failed for {}", encoding.label());
        }

        println!(
            "{},{},{},{},{},{},{},{:.6},{:.6},{:.3},{:.3},{:.3},{:.3},{:.2},{:.2},{:.2},{:.2}",
            corpus.name,
            corpus.blobs.len(),
            raw_bytes,
            object_zstd_bytes,
            encoding.label(),
            if prepared.compressed { "zstd" } else { "raw" },
            prepared.stored.len(),
            prepared.stored.len() as f64 / raw_bytes as f64,
            prepared.stored.len() as f64 / object_zstd_bytes as f64,
            encode_ms,
            decode_all_ms,
            random_id_ms,
            indexed_ms,
            bytes_to_mib(encode_memory.peak_bytes),
            bytes_to_mib(
                encode_memory
                    .peak_bytes
                    .saturating_sub(encode_memory.baseline_bytes)
            ),
            bytes_to_mib(read_memory.peak_bytes),
            bytes_to_mib(
                read_memory
                    .peak_bytes
                    .saturating_sub(read_memory.baseline_bytes)
            ),
        );
    }
    Ok(())
}

fn encode(corpus: &Corpus, encoding: Encoding) -> Result<Prepared> {
    let slices = corpus.blobs.iter().map(Vec::as_slice).collect::<Vec<_>>();
    let logical = if encoding.is_delta() {
        encode_delta_frame(&slices)?
    } else {
        encode_blob_frame(&slices)?
    };
    let logical_len = logical.len();
    let stored = if encoding.uses_zstd() {
        compress_compact_frame(&logical)?
    } else {
        logical
    };
    let compressed = has_zstd_magic(&stored);
    Ok(Prepared {
        stored,
        logical_len,
        compressed,
    })
}

fn logical_bytes(prepared: &Prepared) -> Result<std::borrow::Cow<'_, [u8]>> {
    if prepared.compressed {
        Ok(std::borrow::Cow::Owned(decompress_pack_payload(
            &prepared.stored,
            prepared.logical_len,
        )?))
    } else {
        Ok(std::borrow::Cow::Borrowed(&prepared.stored))
    }
}

fn verify_prepared(
    corpus: &Corpus,
    encoding: Encoding,
    prepared: &Prepared,
    target_id: ContentHash,
) -> Result<()> {
    let actual = read_by_content_id(encoding, prepared, target_id)?;
    let expected = corpus
        .blobs
        .last()
        .ok_or_else(|| anyhow!("corpus {} is empty", corpus.name))?;
    if actual != *expected {
        bail!("{} failed round-trip validation", encoding.label());
    }
    let metric = decode_all_metric(encoding, prepared)?;
    if metric.0 != corpus.blobs.len()
        || metric.1 != corpus.blobs.iter().map(Vec::len).sum::<usize>()
    {
        bail!("{} failed full-decode validation", encoding.label());
    }
    Ok(())
}

fn decode_all_metric(encoding: Encoding, prepared: &Prepared) -> Result<(usize, usize, u8)> {
    let logical = logical_bytes(prepared)?;
    if encoding.is_delta() {
        let blobs = decode_delta_all(&logical)?;
        let total = blobs.iter().map(Vec::len).sum();
        let marker = blobs
            .last()
            .and_then(|blob| blob.last())
            .copied()
            .unwrap_or(0);
        Ok((blobs.len(), total, marker))
    } else {
        let blobs = decode_blob_frame(&logical)?;
        let total = blobs.iter().map(|(_, body)| body.len()).sum();
        let marker = blobs
            .last()
            .and_then(|(_, body)| body.last())
            .copied()
            .unwrap_or(0);
        Ok((blobs.len(), total, marker))
    }
}

fn read_by_content_id(
    encoding: Encoding,
    prepared: &Prepared,
    target_id: ContentHash,
) -> Result<Vec<u8>> {
    let logical = logical_bytes(prepared)?;
    if encoding.is_delta() {
        let layout = parse_delta_frame(&logical)?;
        let mut current = layout
            .sections
            .first()
            .ok_or_else(|| anyhow!("delta frame has no base"))?
            .to_vec();
        if ContentHash::compute_typed("blob", &current) == target_id {
            return Ok(current);
        }
        for (index, delta) in layout.sections.iter().enumerate().skip(1) {
            current = DeltaDecoder::decode(&current, delta, layout.target_lengths[index])?;
            if ContentHash::compute_typed("blob", &current) == target_id {
                return Ok(current);
            }
        }
        bail!("content id not present in delta frame")
    } else {
        decode_blob_frame(&logical)?
            .into_iter()
            .find_map(|(id, body)| (id == target_id).then(|| body.to_vec()))
            .ok_or_else(|| anyhow!("content id not present in HCB2 frame"))
    }
}

fn read_indexed(encoding: Encoding, prepared: &Prepared, ordinal: usize) -> Result<Vec<u8>> {
    let logical = logical_bytes(prepared)?;
    if encoding.is_delta() {
        let layout = parse_delta_frame(&logical)?;
        if ordinal >= layout.sections.len() {
            bail!("delta ordinal {ordinal} is out of range");
        }
        let mut current = layout.sections[0].to_vec();
        for index in 1..=ordinal {
            current = DeltaDecoder::decode(
                &current,
                layout.sections[index],
                layout.target_lengths[index],
            )?;
        }
        Ok(current)
    } else {
        let ranges = parse_current_ranges(&logical)?;
        let range = ranges
            .get(ordinal)
            .ok_or_else(|| anyhow!("HCB2 ordinal {ordinal} is out of range"))?;
        Ok(logical[range.clone()].to_vec())
    }
}

fn encode_delta_frame(blobs: &[&[u8]]) -> Result<Vec<u8>> {
    if blobs.is_empty() {
        bail!("delta frame needs a base blob");
    }
    let count = u32::try_from(blobs.len()).context("too many blobs for prototype delta frame")?;
    let metadata_len = blobs
        .len()
        .checked_mul(16)
        .and_then(|len| len.checked_add(8))
        .ok_or_else(|| anyhow!("delta metadata length overflow"))?;
    let mut frame = vec![0u8; metadata_len];
    frame[..4].copy_from_slice(DELTA_MAGIC);
    frame[4..8].copy_from_slice(&count.to_le_bytes());

    for (index, blob) in blobs.iter().enumerate() {
        let section = if index == 0 {
            (*blob).to_vec()
        } else {
            DeltaEncoder::encode(blobs[index - 1], blob)
        };
        let target_len = u64::try_from(blob.len()).context("target length exceeds u64")?;
        let section_len = u64::try_from(section.len()).context("section length exceeds u64")?;
        let metadata = 8 + index * 16;
        frame[metadata..metadata + 8].copy_from_slice(&target_len.to_le_bytes());
        frame[metadata + 8..metadata + 16].copy_from_slice(&section_len.to_le_bytes());
        frame.extend_from_slice(&section);
    }
    let checksum = blake3::hash(&frame);
    frame.extend_from_slice(checksum.as_bytes());
    Ok(frame)
}

fn parse_delta_frame(bytes: &[u8]) -> Result<DeltaLayout<'_>> {
    let content_len = bytes
        .len()
        .checked_sub(CHECKSUM_LEN)
        .ok_or_else(|| anyhow!("delta frame shorter than checksum"))?;
    let (content, checksum) = bytes.split_at(content_len);
    if content.get(..4) != Some(DELTA_MAGIC) {
        bail!("delta frame magic mismatch");
    }
    if blake3::hash(content).as_bytes() != checksum {
        bail!("delta frame checksum mismatch");
    }
    let count_bytes: [u8; 4] = content
        .get(4..8)
        .ok_or_else(|| anyhow!("delta frame missing count"))?
        .try_into()
        .map_err(|_| anyhow!("delta frame count width mismatch"))?;
    let count = u32::from_le_bytes(count_bytes) as usize;
    if count == 0 {
        bail!("delta frame has no base");
    }
    let payload_start = count
        .checked_mul(16)
        .and_then(|len| len.checked_add(8))
        .ok_or_else(|| anyhow!("delta metadata overflow"))?;
    if payload_start > content.len() {
        bail!("delta metadata is truncated");
    }

    let mut target_lengths = Vec::with_capacity(count);
    let mut section_lengths = Vec::with_capacity(count);
    for index in 0..count {
        let metadata = 8 + index * 16;
        target_lengths.push(read_fixed_u64(content, metadata)?);
        section_lengths.push(read_fixed_u64(content, metadata + 8)?);
    }
    let mut sections = Vec::with_capacity(count);
    let mut offset = payload_start;
    for section_len in section_lengths {
        let end = offset
            .checked_add(section_len)
            .ok_or_else(|| anyhow!("delta section offset overflow"))?;
        let section = content
            .get(offset..end)
            .ok_or_else(|| anyhow!("delta section is truncated"))?;
        sections.push(section);
        offset = end;
    }
    if offset != content.len() {
        bail!("delta frame has trailing payload bytes");
    }
    if sections[0].len() != target_lengths[0] {
        bail!("delta base length does not match metadata");
    }
    Ok(DeltaLayout {
        target_lengths,
        sections,
    })
}

fn read_fixed_u64(bytes: &[u8], offset: usize) -> Result<usize> {
    let raw: [u8; 8] = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| anyhow!("fixed u64 is truncated"))?
        .try_into()
        .map_err(|_| anyhow!("fixed u64 width mismatch"))?;
    usize::try_from(u64::from_le_bytes(raw)).context("fixed u64 exceeds usize")
}

fn decode_delta_all(bytes: &[u8]) -> Result<Vec<Vec<u8>>> {
    let layout = parse_delta_frame(bytes)?;
    let mut blobs = Vec::with_capacity(layout.sections.len());
    blobs.push(layout.sections[0].to_vec());
    for index in 1..layout.sections.len() {
        let target = DeltaDecoder::decode(
            &blobs[index - 1],
            layout.sections[index],
            layout.target_lengths[index],
        )?;
        if target.len() != layout.target_lengths[index] {
            bail!("decoded delta target length mismatch at ordinal {index}");
        }
        blobs.push(target);
    }
    Ok(blobs)
}

fn parse_current_ranges(bytes: &[u8]) -> Result<Vec<std::ops::Range<usize>>> {
    let content_len = bytes
        .len()
        .checked_sub(CHECKSUM_LEN)
        .ok_or_else(|| anyhow!("HCB2 frame shorter than checksum"))?;
    let (content, checksum) = bytes.split_at(content_len);
    if content.get(..4) != Some(b"HCB2") {
        bail!("HCB2 magic mismatch");
    }
    if blake3::hash(content).as_bytes() != checksum {
        bail!("HCB2 checksum mismatch");
    }
    let mut offset = 4;
    let count =
        usize::try_from(read_varint(content, &mut offset)?).context("HCB2 count exceeds usize")?;
    let mut lengths = Vec::with_capacity(count);
    if count > 0 {
        let first = read_varint(content, &mut offset)?;
        let mut previous = i64::try_from(first).context("HCB2 first length exceeds i64")?;
        lengths.push(usize::try_from(first).context("HCB2 first length exceeds usize")?);
        for _ in 1..count {
            let encoded = read_varint(content, &mut offset)?;
            let delta = ((encoded >> 1) as i64) ^ (-((encoded & 1) as i64));
            previous = previous
                .checked_add(delta)
                .ok_or_else(|| anyhow!("HCB2 length delta overflow"))?;
            if previous < 0 {
                bail!("HCB2 length became negative");
            }
            lengths.push(previous as usize);
        }
    }
    let mut ranges = Vec::with_capacity(count);
    for length in lengths {
        let end = offset
            .checked_add(length)
            .ok_or_else(|| anyhow!("HCB2 body offset overflow"))?;
        if end > content.len() {
            bail!("HCB2 body is truncated");
        }
        ranges.push(offset..end);
        offset = end;
    }
    if offset != content.len() {
        bail!("HCB2 frame has trailing bytes");
    }
    Ok(ranges)
}

fn read_varint(bytes: &[u8], offset: &mut usize) -> Result<u64> {
    let mut value = 0u64;
    for shift in (0..=63).step_by(7) {
        let byte = *bytes
            .get(*offset)
            .ok_or_else(|| anyhow!("varint is truncated"))?;
        *offset += 1;
        if shift == 63 && byte > 1 {
            bail!("varint overflow");
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    bail!("varint overflow")
}

fn median_ms<T>(samples: usize, mut operation: impl FnMut() -> Result<T>) -> Result<f64> {
    let mut durations = Vec::with_capacity(samples);
    for _ in 0..samples {
        let start = Instant::now();
        let value = operation()?;
        durations.push(start.elapsed());
        black_box(value);
    }
    durations.sort_unstable();
    Ok(duration_ms(durations[durations.len() / 2]))
}

fn duration_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn measure_rss<T>(operation: impl FnOnce() -> Result<T>) -> Result<RssMeasurement<T>> {
    let baseline_bytes = current_rss_bytes()?;
    let running = Arc::new(AtomicBool::new(true));
    let peak = Arc::new(AtomicU64::new(baseline_bytes));
    let sampler_running = Arc::clone(&running);
    let sampler_peak = Arc::clone(&peak);
    let sampler = thread::spawn(move || {
        while sampler_running.load(Ordering::Relaxed) {
            if let Ok(rss) = current_rss_bytes() {
                sampler_peak.fetch_max(rss, Ordering::Relaxed);
            }
            thread::sleep(Duration::from_micros(500));
        }
    });
    let value = operation();
    let final_rss = current_rss_bytes();
    if let Ok(rss) = final_rss {
        peak.fetch_max(rss, Ordering::Relaxed);
    }
    running.store(false, Ordering::Relaxed);
    sampler
        .join()
        .map_err(|_| anyhow!("RSS sampler thread panicked"))?;
    Ok(RssMeasurement {
        value: value?,
        baseline_bytes,
        peak_bytes: peak.load(Ordering::Relaxed),
    })
}

fn current_rss_bytes() -> Result<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").context("read /proc/self/statm")?;
    let resident_pages = statm
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow!("/proc/self/statm has no resident page count"))?
        .parse::<u64>()
        .context("parse resident page count")?;
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        bail!("sysconf(_SC_PAGESIZE) failed");
    }
    resident_pages
        .checked_mul(page_size as u64)
        .ok_or_else(|| anyhow!("RSS byte count overflow"))
}

fn trim_allocator() {
    #[cfg(target_os = "linux")]
    // SAFETY: malloc_trim has no pointer preconditions and only asks glibc to
    // return unused allocator pages, reducing cross-row RSS carry-over.
    unsafe {
        libc::malloc_trim(0);
    }
}

fn bytes_to_mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_encoding_round_trips_and_supports_indexed_read() {
        let corpus = synthetic_corpus(
            EditPattern::Scattered {
                edits: 8,
                bytes_each: 8,
            },
            8,
        );
        let expected = corpus.blobs.last().expect("newest blob");
        let expected_id = ContentHash::compute_typed("blob", expected);

        for encoding in Encoding::ALL {
            let prepared = encode(&corpus, encoding).expect("encode");
            verify_prepared(&corpus, encoding, &prepared, expected_id).expect("verify");
            assert_eq!(
                read_indexed(encoding, &prepared, corpus.blobs.len() - 1).expect("indexed read"),
                *expected
            );
        }
    }

    #[test]
    fn dissimilar_delta_frame_records_overhead() {
        let corpus = random_corpus(8);
        let prepared = encode(&corpus, Encoding::DeltaRaw).expect("delta encode");
        let raw_bytes = corpus.blobs.iter().map(Vec::len).sum::<usize>();
        assert!(prepared.stored.len() > raw_bytes);
    }
}
