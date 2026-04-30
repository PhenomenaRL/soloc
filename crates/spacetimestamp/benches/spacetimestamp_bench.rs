use criterion::{black_box, criterion_group, criterion_main, Criterion};
use spacetimestamp::schema::{FrameRegistry, SpaceTimestampBuilder};
use spacetimestamp::validation::validate_spacetimestamp_batch;

fn bench_validation(c: &mut Criterion) {
    let num_records = 100_000;
    
    // Set up a FrameRegistry with a couple of custom frames
    let mut reg = FrameRegistry::new_with_namespace("bench_robot");
    reg.add_frame("cam", "ICRF", [0.0; 3], [1.0, 0.0, 0.0, 0.0]);
    reg.add_frame("arm", "cam", [1.0; 3], [1.0, 0.0, 0.0, 0.0]);

    let mut builder = SpaceTimestampBuilder::new(num_records, Some(reg));

    // Create a batch of 100,000 records containing a mix of standard and local frames
    for i in 0..num_records {
        let frame = if i % 3 == 0 { "ICRF" } else if i % 3 == 1 { "cam" } else { "arm" };
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
            None, None,
        );
    }

    let batch = builder.flush();

    // Verify it works outside the loop
    assert!(validate_spacetimestamp_batch(&batch).is_ok());

    c.bench_function("validate_spacetimestamp_batch (100k rows)", |b| {
        b.iter(|| validate_spacetimestamp_batch(black_box(&batch)))
    });
}

criterion_group!(benches, bench_validation);
criterion_main!(benches);
