use anise::prelude::Almanac;
use arrow::datatypes::{DataType, Field, Schema};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use hifitime::{Duration, Epoch};
use nalgebra::{Isometry3, Translation3, UnitQuaternion};
use spacetimestamp::query::{SpatiotemporalFilter, filter_batch};
use spacetimestamp::schema::{SpaceTimestampBuilder, sts_schema};
use spacetimestamp::transforms::{normalize_batch_to_tai, transform_batch};
use spacetimestamp::validation::validate_spacetimestamp_batch;
use std::hint::black_box;
use std::str::FromStr;
use std::sync::Arc;

fn j2000_tai() -> Epoch {
    Epoch::from_str("2000-01-01T12:00:00 TAI").unwrap()
}

/// Wraps SpaceTimestampBuilder output into a RecordBatch with a named struct column,
/// as required by filter_batch.
fn finish_as_wrapped_batch(
    builder: &mut SpaceTimestampBuilder,
) -> arrow::record_batch::RecordBatch {
    let struct_array = builder.finish_as_struct();
    let sts_ref = sts_schema();
    let schema = Arc::new(
        Schema::new(vec![Field::new(
            "spacetimestamp",
            DataType::Struct(sts_ref.fields().clone()),
            false,
        )])
        .with_metadata(sts_ref.metadata().clone()),
    );
    arrow::record_batch::RecordBatch::try_new(schema, vec![Arc::new(struct_array)]).unwrap()
}

/// Build a batch of n_rows on a full LEO orbit, each row 1ms apart.
/// All rows are in ICRF to satisfy the frame-uniformity requirement for spatial filters.
fn make_filter_bench_batch(n_rows: usize) -> arrow::record_batch::RecordBatch {
    let mut builder = SpaceTimestampBuilder::new(n_rows);
    for i in 0..n_rows {
        let angle = (i as f64) * 2.0 * std::f64::consts::PI / (n_rows as f64);
        builder.append_spacetimestamp(
            "ICRF",
            "km",
            "TAI",
            "bench_source",
            "MEASURED",
            [6800.0 * angle.cos(), 6800.0 * angle.sin(), 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            (i as u64) * 1_000_000, // 1 ms per row
            None,
            None,
        );
    }
    finish_as_wrapped_batch(&mut builder)
}

/// Time filter covering the middle 10% of the batch's time range.
fn time_filter_10pct(n_rows: usize) -> SpatiotemporalFilter {
    let j2000 = j2000_tai();
    let total_ns = (n_rows as u64) * 1_000_000;
    let mid = j2000 + Duration::from_parts(0, total_ns / 2);
    let half_window = Duration::from_parts(0, total_ns / 20);
    SpatiotemporalFilter::new().with_time_range(mid - half_window, mid + half_window)
}

/// Spatial filter: sphere centred on the orbit at angle=0, capturing ~19% of a full orbit.
/// Radius chosen so rows within ±0.6 rad of angle=0 pass (≈19% of 2π).
fn spatial_filter_19pct() -> SpatiotemporalFilter {
    SpatiotemporalFilter::new().with_spatial([6800.0, 0.0, 0.0], 4000.0)
}

fn bench_filter_batch_time(c: &mut Criterion) {
    let mut group = c.benchmark_group("filter_batch_time");
    for n_rows in [1_000usize, 10_000, 100_000] {
        let batch = make_filter_bench_batch(n_rows);
        let filter = time_filter_10pct(n_rows);
        group.bench_with_input(BenchmarkId::new("rows", n_rows), &n_rows, |b, _| {
            b.iter(|| filter_batch(black_box(&batch), black_box(&filter)).unwrap())
        });
    }
    group.finish();
}

fn bench_filter_batch_spatial(c: &mut Criterion) {
    let filter = spatial_filter_19pct();
    let mut group = c.benchmark_group("filter_batch_spatial");
    for n_rows in [1_000usize, 10_000, 100_000] {
        let batch = make_filter_bench_batch(n_rows);
        group.bench_with_input(BenchmarkId::new("rows", n_rows), &n_rows, |b, _| {
            b.iter(|| filter_batch(black_box(&batch), black_box(&filter)).unwrap())
        });
    }
    group.finish();
}

fn bench_filter_batch_combined(c: &mut Criterion) {
    let mut group = c.benchmark_group("filter_batch_combined");
    for n_rows in [1_000usize, 10_000, 100_000] {
        let batch = make_filter_bench_batch(n_rows);
        let filter = time_filter_10pct(n_rows).with_spatial([6800.0, 0.0, 0.0], 4000.0);
        group.bench_with_input(BenchmarkId::new("rows", n_rows), &n_rows, |b, _| {
            b.iter(|| filter_batch(black_box(&batch), black_box(&filter)).unwrap())
        });
    }
    group.finish();
}

fn bench_validation(c: &mut Criterion) {
    let num_records = 100_000;

    let mut builder = SpaceTimestampBuilder::new(num_records);

    // Create a batch of 100,000 records mixing an astronomical root with entity-URI frames,
    // so the validator exercises both the anise path and the entity-URI path.
    for i in 0..num_records {
        let frame = if i % 3 == 0 {
            "ICRF"
        } else if i % 3 == 1 {
            "bench_robot:cam"
        } else {
            "bench_robot:arm"
        };
        let timescale = if i % 2 == 0 { "TAI" } else { "UTC" };

        builder.append_spacetimestamp(
            frame,
            "m",
            timescale,
            "sensor_1",
            "MEASURED",
            [i as f64; 3],
            [1.0, 0.0, 0.0, 0.0],
            0,
            i as u64,
            None,
            None,
        );
    }

    let batch = builder.flush();

    // Verify it works outside the loop
    assert!(validate_spacetimestamp_batch(&batch).is_ok());

    c.bench_function("validate_spacetimestamp_batch (100k rows)", |b| {
        b.iter(|| validate_spacetimestamp_batch(black_box(&batch)))
    });
}

fn bench_ingestion(c: &mut Criterion) {
    let mut group = c.benchmark_group("ingestion");
    for n_rows in [1_000usize, 10_000, 100_000] {
        group.bench_with_input(BenchmarkId::new("rows", n_rows), &n_rows, |b, &n| {
            b.iter(|| {
                let mut builder = SpaceTimestampBuilder::new(n);
                for i in 0..n {
                    builder.append_spacetimestamp(
                        "ICRF",
                        "km",
                        "TAI",
                        "sensor_1",
                        "MEASURED",
                        [i as f64, 0.0, 0.0],
                        [1.0, 0.0, 0.0, 0.0],
                        0,
                        i as u64,
                        None,
                        None,
                    );
                }
                builder.flush()
            })
        });
    }
    group.finish();
}

fn make_transform_bench_batch(n_rows: usize) -> arrow::record_batch::RecordBatch {
    let mut builder = SpaceTimestampBuilder::new(n_rows);
    for i in 0..n_rows {
        builder.append_spacetimestamp(
            "bot:arm",
            "km",
            "TAI",
            "s",
            "MEASURED",
            [i as f64, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            i as u64,
            None,
            None,
        );
    }
    let struct_array = builder.finish_as_struct();
    let sts_ref = sts_schema();
    let schema = Arc::new(Schema::new(vec![Field::new(
        "spacetimestamp",
        DataType::Struct(sts_ref.fields().clone()),
        false,
    )]));
    arrow::record_batch::RecordBatch::try_new(schema, vec![Arc::new(struct_array)]).unwrap()
}

fn bench_transform_batch(c: &mut Criterion) {
    // A resolved two-hop chain (arm → base_link → Earth) arrives pre-composed, exactly as
    // Ledger::resolve_to_root supplies it. Almanac::default() suffices because source and
    // target share the same astronomical root.
    // Every row shares one frame and one epoch, so the resolver memo in transform_batch is
    // hit for all but the first row — this measures the per-row transform, not resolution.
    let almanac = Almanac::default();
    let placed =
        Isometry3::from_parts(Translation3::new(1.5, 0.0, 0.0), UnitQuaternion::identity());
    let resolver =
        |frame: &str, _epoch: Epoch| (frame == "bot:arm").then(|| ("Earth".to_string(), placed));

    let mut group = c.benchmark_group("transform_batch_resolved");
    for n_rows in [100usize, 1_000, 10_000] {
        let batch = make_transform_bench_batch(n_rows);
        group.bench_with_input(BenchmarkId::new("rows", n_rows), &n_rows, |b, _| {
            b.iter(|| {
                transform_batch(
                    black_box(&batch),
                    "Earth",
                    black_box(&almanac),
                    "km",
                    Some(&resolver),
                )
                .unwrap()
            })
        });
    }
    group.finish();
}

fn make_normalize_bench_batch(n_rows: usize) -> arrow::record_batch::RecordBatch {
    let mut builder = SpaceTimestampBuilder::new(n_rows);
    for i in 0..n_rows {
        let ts = if i % 2 == 0 { "TAI" } else { "UTC" };
        builder.append_spacetimestamp(
            "ICRF",
            "km",
            ts,
            "s",
            "MEASURED",
            [0.0; 3],
            [1.0, 0.0, 0.0, 0.0],
            0,
            i as u64,
            None,
            None,
        );
    }
    finish_as_wrapped_batch(&mut builder)
}

fn bench_normalize_to_tai(c: &mut Criterion) {
    let mut group = c.benchmark_group("normalize_to_tai");
    for n_rows in [1_000usize, 10_000, 100_000] {
        let batch = make_normalize_bench_batch(n_rows);
        group.bench_with_input(BenchmarkId::new("rows", n_rows), &n_rows, |b, _| {
            b.iter(|| normalize_batch_to_tai(black_box(&batch)).unwrap())
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_ingestion,
    bench_filter_batch_time,
    bench_filter_batch_spatial,
    bench_filter_batch_combined,
    bench_validation,
    bench_transform_batch,
    bench_normalize_to_tai,
);
criterion_main!(benches);
