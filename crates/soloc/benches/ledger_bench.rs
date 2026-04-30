use arrow::record_batch::RecordBatch;
use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use hifitime::{Duration, Epoch};
use soloc::entity::EntityBuilder;
use soloc::ledger::Ledger;
use spacetimestamp::query::SpatiotemporalFilter;
use std::str::FromStr;

fn j2000_tai() -> Epoch {
    Epoch::from_str("2000-01-01T12:00:00 TAI").unwrap()
}

/// One snapshot batch: n_rows entities on a LEO orbit, each 1 ms apart.
/// t_offset_ns shifts the entire batch's timestamps forward in time.
fn make_entity_batch(n_rows: usize, t_offset_ns: u64) -> RecordBatch {
    let mut builder = EntityBuilder::new(n_rows, None);
    for i in 0..n_rows {
        let angle = (i as f64) * 2.0 * std::f64::consts::PI / (n_rows as f64);
        builder.append_entity(
            "urn:soloc:bench_sat",
            "ICRF",
            "km",
            "TAI",
            "urn:soloc:bench_sat",
            "SIMULATED",
            [6800.0 * angle.cos(), 6800.0 * angle.sin(), 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            t_offset_ns + (i as u64) * 1_000_000,
            Some([0.0, 7.8, 0.0]),
            None,
            None,
            Some(500.0),
            None,
        );
    }
    builder.flush()
}

/// Fill a ledger with n_batches snapshots, rows_per_batch rows each.
/// Timestamps are contiguous: batch i starts at i * rows_per_batch * 1_000_000 ns.
fn make_ledger(n_batches: usize, rows_per_batch: usize) -> Ledger {
    let mut ledger = Ledger::new();
    for i in 0..n_batches {
        let t_offset = (i as u64) * (rows_per_batch as u64) * 1_000_000;
        ledger.append(make_entity_batch(rows_per_batch, t_offset));
    }
    ledger
}

/// Time filter capturing the middle 10% of a ledger's total time span.
fn time_filter_10pct(n_batches: usize, rows_per_batch: usize) -> SpatiotemporalFilter {
    let j2000 = j2000_tai();
    let total_ns = (n_batches as u64) * (rows_per_batch as u64) * 1_000_000;
    let mid = j2000 + Duration::from_parts(0, total_ns / 2);
    let half_window = Duration::from_parts(0, total_ns / 20);
    SpatiotemporalFilter::new().with_time_range(mid - half_window, mid + half_window)
}

/// Spatial filter centred on orbit angle=0, ~19% selectivity.
/// Radius of 4000 km from [6800, 0, 0] captures rows within ±0.6 rad of angle=0.
fn spatial_filter_19pct() -> SpatiotemporalFilter {
    SpatiotemporalFilter::new().with_spatial([6800.0, 0.0, 0.0], 4000.0)
}

// ---------------------------------------------------------------------------
// Append: cost of inserting pre-built batches into the ledger.
// RecordBatch is ref-counted so this mostly measures Vec::push overhead.
// ---------------------------------------------------------------------------
fn bench_append(c: &mut Criterion) {
    let batch = make_entity_batch(1_000, 0);
    let mut group = c.benchmark_group("ledger_append");
    for n_batches in [10usize, 100, 1_000] {
        group.bench_with_input(BenchmarkId::new("n_batches", n_batches), &n_batches, |b, &n| {
            b.iter(|| {
                let mut ledger = Ledger::new();
                for _ in 0..n {
                    ledger.append(black_box(batch.clone()));
                }
                ledger
            })
        });
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// Query with only a time filter across three configurations that share the
// same total row count (10 000) but differ in batch-count / rows-per-batch.
// This reveals whether query cost is driven by batch count or row count.
// ---------------------------------------------------------------------------
fn bench_query_time_filter(c: &mut Criterion) {
    let configs: &[(usize, usize)] = &[(10, 1_000), (100, 100), (1_000, 10)];
    let mut group = c.benchmark_group("ledger_query_time_filter");
    for &(n_batches, rows_per_batch) in configs {
        let ledger = make_ledger(n_batches, rows_per_batch);
        let filter = time_filter_10pct(n_batches, rows_per_batch);
        group.bench_with_input(
            BenchmarkId::new("batches_x_rows", format!("{n_batches}x{rows_per_batch}")),
            &(n_batches, rows_per_batch),
            |b, _| b.iter(|| ledger.query(black_box(&filter), "spacetimestamp").unwrap()),
        );
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// Query with only a spatial filter (frame uniformity satisfied; all ICRF).
// ---------------------------------------------------------------------------
fn bench_query_spatial_filter(c: &mut Criterion) {
    let filter = spatial_filter_19pct();
    let configs: &[(usize, usize)] = &[(10, 1_000), (100, 100), (1_000, 10)];
    let mut group = c.benchmark_group("ledger_query_spatial_filter");
    for &(n_batches, rows_per_batch) in configs {
        let ledger = make_ledger(n_batches, rows_per_batch);
        group.bench_with_input(
            BenchmarkId::new("batches_x_rows", format!("{n_batches}x{rows_per_batch}")),
            &(n_batches, rows_per_batch),
            |b, _| b.iter(|| ledger.query(black_box(&filter), "spacetimestamp").unwrap()),
        );
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// Query with both filters active (~2% of rows pass: 10% × 19%).
// Measures whether early-exit on time check reduces spatial work.
// ---------------------------------------------------------------------------
fn bench_query_combined_filter(c: &mut Criterion) {
    let configs: &[(usize, usize)] = &[(10, 1_000), (100, 100), (1_000, 10)];
    let mut group = c.benchmark_group("ledger_query_combined_filter");
    for &(n_batches, rows_per_batch) in configs {
        let ledger = make_ledger(n_batches, rows_per_batch);
        let filter = time_filter_10pct(n_batches, rows_per_batch)
            .with_spatial([6800.0, 0.0, 0.0], 4000.0);
        group.bench_with_input(
            BenchmarkId::new("batches_x_rows", format!("{n_batches}x{rows_per_batch}")),
            &(n_batches, rows_per_batch),
            |b, _| b.iter(|| ledger.query(black_box(&filter), "spacetimestamp").unwrap()),
        );
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// stream_query vs query: streaming avoids final Arrow concat_batches.
// Measures how much the concat step costs on a full (no-filter) scan.
// ---------------------------------------------------------------------------
fn bench_stream_vs_query(c: &mut Criterion) {
    let ledger = make_ledger(100, 100); // 10 000 rows, 100 batches
    let no_filter = SpatiotemporalFilter::new();
    let mut group = c.benchmark_group("ledger_stream_vs_query");
    group.bench_function("query_concat", |b| {
        b.iter(|| ledger.query(black_box(&no_filter), "spacetimestamp").unwrap())
    });
    group.bench_function("stream_query_count", |b| {
        b.iter(|| {
            ledger
                .stream_query(black_box(&no_filter), "spacetimestamp")
                .filter_map(Result::ok)
                .map(|rb| rb.num_rows())
                .sum::<usize>()
        })
    });
    group.finish();
}

// ---------------------------------------------------------------------------
// latest_snapshot: should be O(1) regardless of ledger depth.
// ---------------------------------------------------------------------------
fn bench_latest_snapshot(c: &mut Criterion) {
    let mut group = c.benchmark_group("ledger_latest_snapshot");
    for n_batches in [10usize, 100, 1_000] {
        let ledger = make_ledger(n_batches, 100);
        group.bench_with_input(
            BenchmarkId::new("n_batches", n_batches),
            &n_batches,
            |b, _| b.iter(|| ledger.latest_snapshot(black_box(None))),
        );
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// IPC round-trip: cost of flushing tier-1 (memory) → tier-2 (disk).
// Measures both save and load separately so the split is clear.
// ---------------------------------------------------------------------------
fn bench_ipc_roundtrip(c: &mut Criterion) {
    let configs: &[(usize, usize)] = &[(10, 1_000), (100, 100)];
    let mut group = c.benchmark_group("ledger_ipc");
    for &(n_batches, rows_per_batch) in configs {
        let label = format!("{n_batches}x{rows_per_batch}");
        let ledger = make_ledger(n_batches, rows_per_batch);
        let path = std::env::temp_dir()
            .join(format!("soloc_ledger_bench_{label}.arrows"));

        group.bench_with_input(BenchmarkId::new("save", &label), &label, |b, _| {
            b.iter(|| ledger.save_ipc(black_box(&path)).unwrap())
        });

        // Ensure the file exists before the load bench runs.
        ledger.save_ipc(&path).unwrap();

        group.bench_with_input(BenchmarkId::new("load", &label), &label, |b, _| {
            b.iter(|| Ledger::load_ipc(black_box(&path)).unwrap())
        });

        std::fs::remove_file(&path).ok();
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_append,
    bench_query_time_filter,
    bench_query_spatial_filter,
    bench_query_combined_filter,
    bench_stream_vs_query,
    bench_latest_snapshot,
    bench_ipc_roundtrip,
);
criterion_main!(benches);
