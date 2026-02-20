use criterion::{Criterion, black_box, criterion_group, criterion_main};
use spacetimestamp::{SpaceTimestampBuilder, export_sts_schema_to_file, sts_schema};
use std::fs;

fn bench_schema_definition(c: &mut Criterion) {
    c.bench_function("sts_schema_definition", |b| {
        b.iter(|| black_box(sts_schema()))
    });
}

fn bench_recordbatch_generation(c: &mut Criterion) {
    let schema = sts_schema();
    let row_count = 1000;

    c.bench_function("recordbatch_generation_1000_rows", |b| {
        b.iter(|| {
            let mut builder = SpaceTimestampBuilder::new(row_count);
            for i in 0..row_count {
                builder.append_spacetimestamp(
                    black_box("EME2000"),
                    black_box("km"),
                    black_box("TDB"),
                    black_box("MEASURED"),
                    black_box([i as f64, i as f64 * 10.0, i as f64 * 100.0]),
                    black_box([1.0, 0.0, 0.0, 0.0]),
                    black_box(0),
                    black_box(i as u64),
                );
            }
            black_box(builder.flush(schema.clone()))
        })
    });
}

fn bench_schema_export(c: &mut Criterion) {
    let temp_dir = std::env::temp_dir();
    let file_path = temp_dir.join("bench_sts_schema.arrow");

    c.bench_function("export_sts_schema_to_file", |b| {
        b.iter(|| {
            let _ = export_sts_schema_to_file(black_box(&file_path));
        })
    });

    // Cleanup after benchmark
    if file_path.exists() {
        let _ = fs::remove_file(file_path);
    }
}

criterion_group!(
    benches,
    bench_schema_definition,
    bench_recordbatch_generation,
    bench_schema_export
);
criterion_main!(benches);
