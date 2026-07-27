//! Append-time cost of deriving transform topology from rows.
//!
//! `TransformTree::ingest_batch` runs on every `Ledger::append`, so its per-row cost is
//! added to every ingest in the system. The three scenarios here bracket what a real
//! append can look like:
//!
//! - **steady_state** — the overwhelmingly common case. Entities report new poses but
//!   nobody re-parents, so Pass A + Pass B run and Pass C is skipped entirely.
//! - **first_sighting** — the bootstrap case, and the worst case for Pass C: every id is
//!   dirty, so the sort + ordered walk covers all N rows.
//! - **one_reparent** — a single entity among many changes parent, exercising the
//!   tiering that Pass C's dirty-id filter exists to provide.

use arrow::array::{RecordBatch, StringDictionaryBuilder, StructArray};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef, UInt32Type};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use spacetimestamp::schema::{STS_COLUMN, SpaceTimestampBuilder, sts_schema};
use spacetimestamp::topology::TransformTree;
use std::hint::black_box;
use std::sync::Arc;

/// Entity-shaped schema: an id column plus a nested spacetimestamp struct.
fn bench_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new(
            "entity_id",
            DataType::Dictionary(Box::new(DataType::UInt32), Box::new(DataType::Utf8)),
            false,
        ),
        Field::new(
            STS_COLUMN,
            DataType::Struct(sts_schema(None).fields().clone()),
            false,
        ),
    ]))
}

fn build_batch(rows: &[(String, &str, u64)]) -> RecordBatch {
    let mut ids = StringDictionaryBuilder::<UInt32Type>::new();
    let mut sts = SpaceTimestampBuilder::new(rows.len(), None);
    for (id, frame, ns) in rows {
        ids.append_value(id);
        sts.append_spacetimestamp(
            frame,
            "km",
            "TAI",
            "bench:src",
            "MEASURED",
            [6800.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            *ns,
            None,
            None,
        );
    }
    let sts_array: StructArray = sts.finish_as_struct();
    RecordBatch::try_new(
        bench_schema(),
        vec![Arc::new(ids.finish()), Arc::new(sts_array)],
    )
    .unwrap()
}

/// `n_rows` spread over `n_entities`, one timestep per round of entities.
/// All rows are parented to "ICRF" — no topology change.
fn steady_state_batch(n_rows: usize, n_entities: usize) -> RecordBatch {
    let rows: Vec<(String, &str, u64)> = (0..n_rows)
        .map(|i| {
            (
                format!("bench:sat_{}", i % n_entities),
                "ICRF",
                (i / n_entities) as u64 * 1_000_000,
            )
        })
        .collect();
    build_batch(&rows)
}

/// Same shape, but entity 0 re-parents to another entity partway through the batch.
fn one_reparent_batch(n_rows: usize, n_entities: usize) -> RecordBatch {
    let rows: Vec<(String, &str, u64)> = (0..n_rows)
        .map(|i| {
            let entity = i % n_entities;
            let step = (i / n_entities) as u64;
            let frame = if entity == 0 && i > n_rows / 2 {
                "bench:sat_1"
            } else {
                "ICRF"
            };
            (format!("bench:sat_{entity}"), frame, step * 1_000_000)
        })
        .collect();
    build_batch(&rows)
}

/// One row per entity — the shape a real ledger snapshot has, and the case where
/// Pass B's k-bounded work (a `String` per distinct id, on every append) dominates.
/// Against a cold tree this is also the Pass C worst case: every id is dirty.
fn first_sighting_batch(n_rows: usize) -> RecordBatch {
    let rows: Vec<(String, &str, u64)> = (0..n_rows)
        .map(|i| (format!("bench:sat_{i}"), "ICRF", i as u64 * 1_000_000))
        .collect();
    build_batch(&rows)
}

fn bench_ingest(c: &mut Criterion) {
    const SIZES: &[usize] = &[1_000, 10_000, 100_000, 500_000];
    const ENTITIES: usize = 100;

    let mut group = c.benchmark_group("topology_ingest");
    group.sample_size(20);

    for &n in SIZES {
        // Steady state: tree is pre-warmed, so no id is dirty and Pass C is skipped.
        let batch = steady_state_batch(n, ENTITIES);
        let mut warm = TransformTree::new();
        warm.ingest_batch(&batch, "entity_id").unwrap();
        group.bench_with_input(BenchmarkId::new("steady_state", n), &n, |b, _| {
            b.iter_batched(
                || warm.clone(),
                |mut tree| {
                    let outcome = tree.ingest_batch(&batch, "entity_id").unwrap();
                    // Return the tree so criterion drops it outside the measured region;
                    // tearing down a large tree costs more than the ingest itself.
                    black_box((tree, outcome))
                },
                criterion::BatchSize::SmallInput,
            )
        });

        // One entity re-parents: Pass C runs, but only over that entity's rows.
        let batch = one_reparent_batch(n, ENTITIES);
        let mut warm = TransformTree::new();
        warm.ingest_batch(&steady_state_batch(n, ENTITIES), "entity_id")
            .unwrap();
        group.bench_with_input(BenchmarkId::new("one_reparent", n), &n, |b, _| {
            b.iter_batched(
                || warm.clone(),
                |mut tree| {
                    let outcome = tree.ingest_batch(&batch, "entity_id").unwrap();
                    // Return the tree so criterion drops it outside the measured region;
                    // tearing down a large tree costs more than the ingest itself.
                    black_box((tree, outcome))
                },
                criterion::BatchSize::SmallInput,
            )
        });

        // First sighting: every id dirty, Pass C sorts and walks all N rows.
        let batch = first_sighting_batch(n);
        group.bench_with_input(BenchmarkId::new("first_sighting", n), &n, |b, _| {
            b.iter_batched(
                TransformTree::new,
                |mut tree| {
                    let outcome = tree.ingest_batch(&batch, "entity_id").unwrap();
                    // Return the tree so criterion drops it outside the measured region;
                    // tearing down a large tree costs more than the ingest itself.
                    black_box((tree, outcome))
                },
                criterion::BatchSize::SmallInput,
            )
        });

        // The recurring cost of that same shape once the tree is warm: no events, but
        // Pass B still touches every distinct id. This is what a per-timestep snapshot
        // append of n entities actually costs.
        let mut warm = TransformTree::new();
        warm.ingest_batch(&batch, "entity_id").unwrap();
        group.bench_with_input(BenchmarkId::new("steady_state_wide", n), &n, |b, _| {
            b.iter_batched(
                || warm.clone(),
                |mut tree| {
                    let outcome = tree.ingest_batch(&batch, "entity_id").unwrap();
                    // Return the tree so criterion drops it outside the measured region;
                    // tearing down a large tree costs more than the ingest itself.
                    black_box((tree, outcome))
                },
                criterion::BatchSize::SmallInput,
            )
        });
    }

    group.finish();
}

criterion_group!(benches, bench_ingest);
criterion_main!(benches);
