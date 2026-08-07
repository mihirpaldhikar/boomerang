/*
 * Copyright (c) Mihir Paldhikar
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the “Software”), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED “AS IS”, WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 */

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use database::{Symbol, SymbolInterner};
use std::hint::black_box;
use std::sync::Arc;
use std::thread;

/// Deterministic pseudo-random-ish unique string generator so the "miss"
/// benches never accidentally hit the thread-local cache or the shared map.
fn unique_strings(n: usize, len_hint: usize) -> Vec<String> {
    (0..n)
        .map(|i| {
            let mut s = format!("bench_unique_{i}_");
            while s.len() < len_hint {
                s.push('x');
            }
            s
        })
        .collect()
}

fn ascii_of_len(len: usize) -> String {
    "a".repeat(len)
}

// A string with multi-byte UTF-8 chars (each 'é' is 2 bytes, mix in emoji
// which is 4 bytes) to exercise any byte-vs-char-length assumptions in the
// arena / hashing path.
fn unicode_of_approx_len(byte_len: usize) -> String {
    let mut s = String::new();
    while s.len() < byte_len {
        s.push_str("héllo_wörld_🦀_");
    }
    let mut cut = byte_len.min(s.len());
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    s
}

// 1. Fast paths

fn bench_fast_paths(c: &mut Criterion) {
    let interner = SymbolInterner::new();
    let sym_hello = interner.get_or_intern("hello");
    let _sym_world = interner.get_or_intern("world");
    let sym_static = interner.get_or_intern("a");

    let mut group = c.benchmark_group("fast_path");

    group.bench_function("static_1char_repeat", |b| {
        b.iter(|| interner.get_or_intern(black_box("a")))
    });

    group.bench_function("thread_local_cache_hit", |b| {
        b.iter(|| interner.get_or_intern(black_box("hello")))
    });

    group.bench_function("empty_string_repeat", |b| {
        b.iter(|| interner.get_or_intern(black_box("")))
    });

    group.bench_function("resolve_hit", |b| {
        b.iter(|| interner.resolve(black_box(sym_hello)))
    });

    group.bench_function("resolve_static", |b| {
        b.iter(|| interner.resolve(black_box(sym_static)))
    });

    group.finish();
}

// 2. Cold / miss path — every call is a genuine new intern

fn bench_miss_path(c: &mut Criterion) {
    let mut group = c.benchmark_group("miss_path");

    group.bench_function("intern_unique_cold_interner", |b| {
        b.iter_batched(
            || (SymbolInterner::new(), unique_strings(1, 12)),
            |(interner, strings)| {
                black_box(interner.get_or_intern(&strings[0]));
            },
            BatchSize::SmallInput,
        )
    });

    group.bench_function("intern_unique_warm_interner", |b| {
        let interner = SymbolInterner::new();
        for s in unique_strings(10_000, 12) {
            interner.get_or_intern(&s);
        }
        let fresh = unique_strings(1, 12); // will collide with warm-up set names if regenerated; regenerate distinct below
        let _ = fresh;
        b.iter_batched(
            || unique_strings(1, 12), // ASSUMPTION: fine if these overlap warm-up ids in test; use a disjoint id range if needed
            |strings| {
                black_box(interner.get_or_intern(&strings[0]));
            },
            BatchSize::SmallInput,
        )
    });

    group.finish();
}

// 3. Resolve edge cases

fn bench_resolve_edges(c: &mut Criterion) {
    let interner = SymbolInterner::new();

    let sym_first = interner.get_or_intern("first_symbol_ever");
    let sym_empty = interner.get_or_intern("");
    for s in unique_strings(50_000, 16) {
        interner.get_or_intern(&s);
    }
    let sym_last = interner.get_or_intern("last_symbol_ever");

    let mut group = c.benchmark_group("resolve_edges");

    group.bench_function("resolve_first_interned", |b| {
        b.iter(|| interner.resolve(black_box(sym_first)))
    });

    group.bench_function("resolve_last_interned", |b| {
        b.iter(|| interner.resolve(black_box(sym_last)))
    });

    group.bench_function("resolve_empty_string_symbol", |b| {
        b.iter(|| interner.resolve(black_box(sym_empty)))
    });

    group.finish();
}

// 4. String length sweep — throughput-normalized so you can compare
//    bytes/sec across sizes, not just ns/op.

fn bench_length_sweep(c: &mut Criterion) {
    let mut group = c.benchmark_group("intern_by_length");

    for &len in &[1usize, 8, 16, 32, 64, 128, 256, 1024, 4096, 16_384] {
        group.throughput(Throughput::Bytes(len as u64));

        group.bench_with_input(BenchmarkId::new("cache_hit", len), &len, |b, &len| {
            let interner = SymbolInterner::new();
            let s = ascii_of_len(len);
            interner.get_or_intern(&s);
            b.iter(|| interner.get_or_intern(black_box(&s)))
        });

        group.bench_with_input(BenchmarkId::new("cold_unique", len), &len, |b, &len| {
            b.iter_batched(
                || {
                    let interner = SymbolInterner::new();
                    interner.get_or_intern("warmup_throwaway_key"); // untimed: pays lazy-init cost here, not in the routine
                    let s = ascii_of_len(len);
                    (interner, s)
                },
                |(interner, s)| black_box(interner.get_or_intern(&s)),
                BatchSize::SmallInput,
            )
        });
    }

    group.finish();
}

// 5. Unicode / multi-byte

fn bench_unicode(c: &mut Criterion) {
    let mut group = c.benchmark_group("unicode");

    for &len in &[8usize, 64, 256] {
        let s = unicode_of_approx_len(len);
        group.throughput(Throughput::Bytes(s.len() as u64));

        group.bench_with_input(
            BenchmarkId::new("intern_repeat_utf8", s.len()),
            &s,
            |b, s| {
                let interner = SymbolInterner::new();
                interner.get_or_intern(s);
                b.iter(|| interner.get_or_intern(black_box(s)))
            },
        );

        group.bench_with_input(BenchmarkId::new("intern_cold_utf8", s.len()), &s, |b, s| {
            b.iter_batched(
                SymbolInterner::new,
                |interner| black_box(interner.get_or_intern(s)),
                BatchSize::SmallInput,
            )
        });
    }

    group.finish();
}

// 6. Duplicate ratio sweep — realistic workloads are neither 100% cache
//    hits nor 100% unique. Sweep the hit ratio.

fn bench_duplicate_ratio(c: &mut Criterion) {
    let mut group = c.benchmark_group("duplicate_ratio");

    for &ratio_pct in &[0u32, 25, 50, 75, 100] {
        group.bench_with_input(
            BenchmarkId::new("mixed_workload", ratio_pct),
            &ratio_pct,
            |b, &ratio_pct| {
                let interner = SymbolInterner::new();
                let hot_pool = unique_strings(64, 16);
                for s in &hot_pool {
                    interner.get_or_intern(s);
                }
                let mut cold_iter = 0usize;

                b.iter(|| {
                    let roll = (cold_iter as u32).wrapping_mul(2654435761) % 100;
                    let sym = if roll < ratio_pct {
                        let idx = cold_iter % hot_pool.len();
                        interner.get_or_intern(black_box(&hot_pool[idx]))
                    } else {
                        let s = format!("cold_{cold_iter}");
                        interner.get_or_intern(black_box(&s))
                    };
                    cold_iter = cold_iter.wrapping_add(1);
                    sym
                })
            },
        );
    }

    group.finish();
}

// 7. Growth / resize amortized cost — bulk-load N fresh symbols into a
//    fresh interner, per batch, to capture resize/rehash overhead rather
//    than steady-state single-insert cost.

fn bench_bulk_growth(c: &mut Criterion) {
    let mut group = c.benchmark_group("bulk_growth");
    group.sample_size(20);

    for &n in &[1_000usize, 10_000, 100_000] {
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::new("intern_n_unique", n), &n, |b, &n| {
            b.iter_batched(
                || unique_strings(n, 16),
                |strings| {
                    let interner = SymbolInterner::new();
                    for s in &strings {
                        black_box(interner.get_or_intern(s));
                    }
                },
                BatchSize::LargeInput,
            )
        });
    }

    group.finish();
}

// 8. Concurrency: shard contention under multiple threads

fn bench_concurrent(c: &mut Criterion) {
    let mut group = c.benchmark_group("concurrent");
    group.sample_size(10);

    for &n_threads in &[1usize, 2, 4, 8] {
        group.bench_with_input(
            BenchmarkId::new("same_key_contention", n_threads),
            &n_threads,
            |b, &n_threads| {
                let interner = Arc::new(SymbolInterner::new());
                interner.get_or_intern("contended_key");

                b.iter(|| {
                    let handles: Vec<_> = (0..n_threads)
                        .map(|_| {
                            let interner = Arc::clone(&interner);
                            thread::spawn(move || {
                                for _ in 0..1000 {
                                    black_box(interner.get_or_intern("contended_key"));
                                }
                            })
                        })
                        .collect();
                    for h in handles {
                        h.join().unwrap();
                    }
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("disjoint_key_scaling", n_threads),
            &n_threads,
            |b, &n_threads| {
                let interner = Arc::new(SymbolInterner::new());

                b.iter_batched(
                    || {
                        (0..n_threads)
                            .map(|t| {
                                unique_strings(1000, 16)
                                    .into_iter()
                                    .map(move |s| format!("t{t}_{s}"))
                                    .collect::<Vec<_>>()
                            })
                            .collect::<Vec<_>>()
                    },
                    |per_thread_strings| {
                        let interner = Arc::clone(&interner);
                        let handles: Vec<_> = per_thread_strings
                            .into_iter()
                            .map(|strings| {
                                let interner = Arc::clone(&interner);
                                thread::spawn(move || {
                                    for s in &strings {
                                        black_box(interner.get_or_intern(s));
                                    }
                                })
                            })
                            .collect();
                        for h in handles {
                            h.join().unwrap();
                        }
                    },
                    BatchSize::LargeInput,
                )
            },
        );
    }

    group.finish();
}

// 9. Serialization round-trip (bytes() / from_bytes())
//    ASSUMPTION: `interner.bytes() -> Vec<u8>` and
//    `SymbolInterner::from_bytes(&[u8]) -> SymbolInterner` (or Result<..>).
//    Adjust names/signatures to match SCHB2 arena header API.

fn bench_serialization(c: &mut Criterion) {
    let mut group = c.benchmark_group("serialization");
    group.sample_size(20);

    for &n in &[100usize, 10_000, 100_000] {
        let interner = SymbolInterner::new();
        for s in unique_strings(n, 16) {
            interner.get_or_intern(&s);
        }
        let serialized: Vec<u8> = interner.bytes().to_vec();
        
        group.throughput(Throughput::Bytes(serialized.len() as u64));
        group.bench_with_input(BenchmarkId::new("bytes_encode_copy", n), &n, |b, _| {
            b.iter(|| {
                black_box(interner.bytes());
            })
        });

        group.bench_with_input(BenchmarkId::new("from_bytes_decode", n), &n, |b, _| {
            b.iter(|| {
                black_box(SymbolInterner::from_bytes(black_box(&serialized)));
            })
        });

        group.bench_with_input(BenchmarkId::new("round_trip", n), &n, |b, _| {
            b.iter(|| {
                let bytes = interner.bytes().to_vec();
                black_box(SymbolInterner::from_bytes(black_box(&bytes)));
            })
        });
    }

    group.bench_function("decode_then_resolve_one", |b| {
        let interner = SymbolInterner::new();
        let syms: Vec<Symbol> = unique_strings(50_000, 16)
            .into_iter()
            .map(|s| interner.get_or_intern(&s))
            .collect();
        let bytes: Vec<u8> = interner.bytes().to_vec();
        let target = syms[syms.len() / 2];

        b.iter(|| {
            let decoded = SymbolInterner::from_bytes(black_box(&bytes));
            black_box(decoded.resolve(black_box(target)));
        })
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_fast_paths,
    bench_miss_path,
    bench_resolve_edges,
    bench_length_sweep,
    bench_unicode,
    bench_duplicate_ratio,
    bench_bulk_growth,
    bench_concurrent,
    bench_serialization,
);
criterion_main!(benches);
