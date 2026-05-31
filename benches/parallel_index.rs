//! Benchmark for the M6 parallel initial char/line index.
//!
//! Times the one O(filesize) scan `Quire::with_original` does at open time —
//! counting chars and `\n`s over the whole original — sequentially versus the
//! scoped-thread parallel driver, on a large synthetic multibyte input. This is
//! the real cost of opening a multi-GB file; the parallel driver should beat the
//! sequential scan by roughly the available core count (memory-bandwidth bound).
//!
//! Run with `cargo bench --bench parallel_index`. It is NOT compiled or run by
//! `cargo test`, so normal CI stays fast.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use mime_rs::quire::{count_chars_lines, count_chars_lines_parallel};
use std::hint::black_box;

/// Build a realistic UTF-8 corpus: mixed 1/2/3/4-byte scalars and frequent
/// newlines, repeated to the requested byte size. Mirrors the kind of prose /
/// markdown / source mix mime-rs opens.
fn corpus(min_bytes: usize) -> String {
    let unit = "The quick brown fox — αβγ 世界 𝄞 — jumps over café naïve lines.\n\
                Lorem ipsum dolor sit amet, consectetur adipiscing — 日本語テスト。\n";
    let reps = min_bytes.div_ceil(unit.len());
    unit.repeat(reps)
}

fn bench_initial_index(c: &mut Criterion) {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let mut group = c.benchmark_group("initial_char_line_index");

    // A few sizes spanning the threshold (256 KiB) up to a large file. Each is a
    // single full scan; parallel should pull ahead as the input grows.
    for size in [256 * 1024usize, 8 * 1024 * 1024, 64 * 1024 * 1024] {
        let text = corpus(size);
        let bytes = text.as_bytes();
        let len = bytes.len();
        group.throughput(Throughput::Bytes(len as u64));

        // Sanity: both paths agree before we trust the timings.
        assert_eq!(count_chars_lines(bytes), count_chars_lines_parallel(bytes));

        group.bench_with_input(BenchmarkId::new("sequential", len), &bytes, |b, bytes| {
            b.iter(|| black_box(count_chars_lines(black_box(bytes))));
        });
        group.bench_with_input(
            BenchmarkId::new(format!("parallel_{cores}t"), len),
            &bytes,
            |b, bytes| {
                b.iter(|| black_box(count_chars_lines_parallel(black_box(bytes))));
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_initial_index);
criterion_main!(benches);
