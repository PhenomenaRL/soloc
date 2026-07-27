use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use hifitime::{Duration, Epoch};
use soloc::entity::{EntityBuilder, entity_schema};
use soloc::ledger::Ledger;
use spacetimestamp::ephemeris::j2000_tai;
use spacetimestamp::query::SpatiotemporalFilter;
use std::hint::black_box;

fn j2000_epoch() -> Epoch {
    j2000_tai()
}

/// One snapshot batch: n_rows entities on a LEO orbit, each 1 ms apart.
fn make_entity_batch(n_rows: usize, t_offset_ns: u64) -> arrow::record_batch::RecordBatch {
    let mut builder = EntityBuilder::new(n_rows);
    for i in 0..n_rows {
        let angle = (i as f64) * 2.0 * std::f64::consts::PI / (n_rows as f64);
        builder.append_entity(
            "demo:bench_sat",
            "ICRF",
            "km",
            "TAI",
            "demo:bench_sat",
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

fn make_ledger(n_batches: usize, rows_per_batch: usize) -> Ledger {
    let mut ledger = Ledger::new(&entity_schema(), "entity_id").unwrap();
    for i in 0..n_batches {
        let t_offset = (i as u64) * (rows_per_batch as u64) * 1_000_000;
        ledger
            .append(make_entity_batch(rows_per_batch, t_offset))
            .unwrap();
    }
    ledger
}

fn time_filter_10pct(n_batches: usize, rows_per_batch: usize) -> SpatiotemporalFilter {
    let j2000 = j2000_epoch();
    let total_ns = (n_batches as u64) * (rows_per_batch as u64) * 1_000_000;
    let mid = j2000 + Duration::from_parts(0, total_ns / 2);
    let half_window = Duration::from_parts(0, total_ns / 20);
    SpatiotemporalFilter::new().with_time_range(mid - half_window, mid + half_window)
}

fn spatial_filter_19pct() -> SpatiotemporalFilter {
    SpatiotemporalFilter::new().with_spatial([6800.0, 0.0, 0.0], 4000.0)
}

fn bench_entity_ingestion(c: &mut Criterion) {
    let mut group = c.benchmark_group("entity_ingestion");
    for n_rows in [1_000usize, 10_000, 100_000] {
        group.bench_with_input(BenchmarkId::new("rows", n_rows), &n_rows, |b, &n| {
            b.iter(|| {
                let mut builder = soloc::entity::EntityBuilder::new(n);
                for i in 0..n {
                    let angle = (i as f64) * 2.0 * std::f64::consts::PI / (n as f64);
                    builder.append_entity(
                        "demo:sat",
                        "ICRF",
                        "km",
                        "TAI",
                        "demo:src",
                        "MEASURED",
                        [6800.0 * angle.cos(), 6800.0 * angle.sin(), 0.0],
                        [1.0, 0.0, 0.0, 0.0],
                        0,
                        i as u64,
                        Some([0.0, 7.8, 0.0]),
                        None,
                        None,
                        Some(500.0),
                        None,
                    );
                }
                builder.flush()
            })
        });
    }
    group.finish();
}

fn bench_append(c: &mut Criterion) {
    let batch = make_entity_batch(1_000, 0);
    let mut group = c.benchmark_group("ledger_append");
    for n_batches in [10usize, 100, 1_000] {
        group.bench_with_input(
            BenchmarkId::new("n_batches", n_batches),
            &n_batches,
            |b, &n| {
                b.iter(|| {
                    let mut ledger = Ledger::new(&entity_schema(), "entity_id").unwrap();
                    for _ in 0..n {
                        ledger.append(black_box(batch.clone())).unwrap();
                    }
                    ledger
                })
            },
        );
    }
    group.finish();
}

fn bench_query_time_filter(c: &mut Criterion) {
    let configs: &[(usize, usize)] = &[(10, 1_000), (100, 100), (1_000, 10)];
    let mut group = c.benchmark_group("ledger_query_time_filter");
    for &(n_batches, rows_per_batch) in configs {
        let ledger = make_ledger(n_batches, rows_per_batch);
        let filter = time_filter_10pct(n_batches, rows_per_batch);
        group.bench_with_input(
            BenchmarkId::new("batches_x_rows", format!("{n_batches}x{rows_per_batch}")),
            &(n_batches, rows_per_batch),
            |b, _| b.iter(|| ledger.query(black_box(&filter)).unwrap()),
        );
    }
    group.finish();
}

fn bench_query_spatial_filter(c: &mut Criterion) {
    let filter = spatial_filter_19pct();
    let configs: &[(usize, usize)] = &[(10, 1_000), (100, 100), (1_000, 10)];
    let mut group = c.benchmark_group("ledger_query_spatial_filter");
    for &(n_batches, rows_per_batch) in configs {
        let ledger = make_ledger(n_batches, rows_per_batch);
        group.bench_with_input(
            BenchmarkId::new("batches_x_rows", format!("{n_batches}x{rows_per_batch}")),
            &(n_batches, rows_per_batch),
            |b, _| b.iter(|| ledger.query(black_box(&filter)).unwrap()),
        );
    }
    group.finish();
}

fn bench_query_combined_filter(c: &mut Criterion) {
    let configs: &[(usize, usize)] = &[(10, 1_000), (100, 100), (1_000, 10)];
    let mut group = c.benchmark_group("ledger_query_combined_filter");
    for &(n_batches, rows_per_batch) in configs {
        let ledger = make_ledger(n_batches, rows_per_batch);
        let filter =
            time_filter_10pct(n_batches, rows_per_batch).with_spatial([6800.0, 0.0, 0.0], 4000.0);
        group.bench_with_input(
            BenchmarkId::new("batches_x_rows", format!("{n_batches}x{rows_per_batch}")),
            &(n_batches, rows_per_batch),
            |b, _| b.iter(|| ledger.query(black_box(&filter)).unwrap()),
        );
    }
    group.finish();
}

fn bench_stream_vs_query(c: &mut Criterion) {
    let ledger = make_ledger(100, 100);
    let no_filter = SpatiotemporalFilter::new();
    let mut group = c.benchmark_group("ledger_stream_vs_query");
    group.bench_function("query_concat", |b| {
        b.iter(|| ledger.query(black_box(&no_filter)).unwrap())
    });
    group.bench_function("stream_query_count", |b| {
        b.iter(|| {
            ledger
                .stream_query(black_box(&no_filter))
                .filter_map(Result::ok)
                .map(|rb| rb.num_rows())
                .sum::<usize>()
        })
    });
    group.finish();
}

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

fn bench_ipc_roundtrip(c: &mut Criterion) {
    let configs: &[(usize, usize)] = &[(10, 1_000), (100, 100)];
    let mut group = c.benchmark_group("ledger_ipc");
    for &(n_batches, rows_per_batch) in configs {
        let label = format!("{n_batches}x{rows_per_batch}");
        let ledger = make_ledger(n_batches, rows_per_batch);
        let path = std::env::temp_dir().join(format!("soloc_ledger_bench_{label}.arrows"));

        group.bench_with_input(BenchmarkId::new("save", &label), &label, |b, _| {
            b.iter(|| ledger.save_ipc(black_box(&path)).unwrap())
        });

        ledger.save_ipc(&path).unwrap();

        group.bench_with_input(BenchmarkId::new("load", &label), &label, |b, _| {
            b.iter(|| Ledger::load_ipc(black_box(&path), "entity_id").unwrap())
        });

        std::fs::remove_file(&path).ok();
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_entity_ingestion,
    bench_append,
    bench_query_time_filter,
    bench_query_spatial_filter,
    bench_query_combined_filter,
    bench_stream_vs_query,
    bench_latest_snapshot,
    bench_ipc_roundtrip,
);
criterion_main!(benches);
