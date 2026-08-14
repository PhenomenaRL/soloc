//! Proves `spacetimestamp` is usable on its own, with no ledger and no `soloc` dependency.
//!
//! This is the acceptance criterion for the crate split: everything needed to go from raw
//! measurements to a reprojected batch — build, validate, derive topology, transform — must
//! be reachable through this crate's public API alone. Living in `tests/` rather than `src/`
//! is deliberate: an integration test can only touch `pub` items, so if this compiles, an
//! external user can do the same thing.
//!
//! The scenario is the motivating one: a robot on a truck at a facility on Earth, each pose
//! recorded in its parent's frame, transformed ad-hoc into an astronomical frame. The caller
//! supplies the poses (that is what a ledger would otherwise do); this crate supplies the
//! structure and the physics.

use anise::prelude::{Almanac, Epoch};
use arrow::array::{Array, FixedSizeListArray, Float64Array, RecordBatch, StructArray};
use nalgebra::{Isometry3, Translation3, UnitQuaternion};
use std::collections::HashMap;

use spacetimestamp::ephemeris::j2000_tai;
use spacetimestamp::schemas::entity::EntityBuilder;
use spacetimestamp::topology::TransformTree;
use spacetimestamp::transforms::transform_batch;
use spacetimestamp::validation::validate_spacetimestamp_batch;

/// `(entity_id, parent_frame, position_km)` for the three-deep chain under test.
///
/// Each pose is expressed in its parent's frame, so composing the chain must yield
/// `[1000, 20, 3]` km for the robot in the Earth frame.
const RIG: &[(&str, &str, [f64; 3])] = &[
    ("demo:facility", "Earth", [1000.0, 0.0, 0.0]),
    ("demo:truck", "demo:facility", [0.0, 20.0, 0.0]),
    ("demo:robot", "demo:truck", [0.0, 0.0, 3.0]),
];

fn build_batch() -> RecordBatch {
    let mut builder = EntityBuilder::new(RIG.len());
    for (id, frame, position) in RIG {
        builder.append_entity(
            id,
            frame,
            "km",
            "TAI",
            "demo:rig",
            "MEASURED",
            *position,
            [1.0, 0.0, 0.0, 0.0],
            0,
            0,
            None,
            None,
            None,
            None,
            None,
            None,
        );
    }
    builder.flush()
}

/// The caller's own pose store — the role a ledger plays in the `soloc` stack.
fn pose_map() -> HashMap<String, Isometry3<f64>> {
    RIG.iter()
        .map(|(id, _, position)| {
            (
                id.to_string(),
                Isometry3::from_parts(
                    Translation3::new(position[0], position[1], position[2]),
                    UnitQuaternion::identity(),
                ),
            )
        })
        .collect()
}

fn read_position(batch: &RecordBatch, row: usize) -> [f64; 3] {
    let sts = batch
        .column_by_name("spacetimestamp")
        .unwrap()
        .as_any()
        .downcast_ref::<StructArray>()
        .unwrap();
    let list = sts
        .column_by_name("position")
        .unwrap()
        .as_any()
        .downcast_ref::<FixedSizeListArray>()
        .unwrap();
    let values = list
        .values()
        .as_any()
        .downcast_ref::<Float64Array>()
        .unwrap();
    let base = (list.offset() + row) * 3;
    [
        values.value(base),
        values.value(base + 1),
        values.value(base + 2),
    ]
}

#[test]
fn build_validate_derive_topology_and_transform_without_a_ledger() {
    let batch = build_batch();
    let poses = pose_map();
    let epoch = j2000_tai();

    // 1. Validate — frame and timescale identifiers are well-formed.
    validate_spacetimestamp_batch(&batch).expect("rig batch should validate");

    // 2. Derive topology straight from the rows. No registration step, no ledger.
    let mut tree = TransformTree::new();
    let outcome = tree
        .ingest_batch(&batch, "entity_id")
        .expect("topology should derive from an entity batch");
    assert_eq!(
        outcome.events.len(),
        3,
        "one edge per entity on first sight"
    );
    assert_eq!(tree.current_parent("demo:robot"), Some("demo:truck"));

    // 3. Walk the structure to its astronomical anchor.
    let chain = tree
        .resolve_chain("demo:robot", epoch - j2000_tai())
        .expect("robot chain should reach an astronomical root");
    assert_eq!(
        chain,
        vec!["demo:robot", "demo:truck", "demo:facility", "Earth"]
    );

    // 4. Compose per-hop poses into an isometry for each entity frame. This is the caller's
    //    job — the tree supplies structure only, never a pose value.
    let resolve = |frame: &str, at: Epoch| -> Option<(String, Isometry3<f64>)> {
        let chain = tree.resolve_chain(frame, at - j2000_tai()).ok()?;
        let (root, hops) = chain.split_last()?;
        // Walk inward from the root so each hop composes onto its parent's accumulated pose.
        let mut acc = Isometry3::identity();
        for node in hops.iter().rev() {
            acc *= poses.get(node)?;
        }
        Some((root.clone(), acc))
    };

    // 5. Transform every row into the Earth frame in one pass. An empty almanac suffices
    //    because the chain already terminates at the target frame — no kernels needed.
    let result = transform_batch(&batch, "Earth", &Almanac::default(), "km", Some(&resolve))
        .expect("transform should succeed");

    assert_eq!(result.num_rows(), 3);
    // Row 0 is already in Earth frame and passes through untouched.
    assert_positions_match(read_position(&result, 0), [1000.0, 0.0, 0.0], "facility");
    // Rows 1 and 2 compose one and two hops respectively.
    assert_positions_match(read_position(&result, 1), [1000.0, 20.0, 0.0], "truck");
    assert_positions_match(read_position(&result, 2), [1000.0, 20.0, 3.0], "robot");
}

#[test]
fn topology_rejects_an_unreachable_frame_without_a_ledger() {
    // The append-time typo check is part of the standalone path too, not a ledger feature.
    let mut builder = EntityBuilder::new(1);
    builder.append_entity(
        "demo:sat",
        "NOT_A_FRAME",
        "km",
        "TAI",
        "demo:rig",
        "MEASURED",
        [0.0, 0.0, 0.0],
        [1.0, 0.0, 0.0, 0.0],
        0,
        0,
        None,
        None,
        None,
        None,
        None,
        None,
    );
    let batch = builder.flush();

    let mut tree = TransformTree::new();
    let err = tree.ingest_batch(&batch, "entity_id").unwrap_err();
    assert!(err.contains("NOT_A_FRAME"), "error should name it: {err}");
    assert!(
        tree.is_empty(),
        "a rejected batch must leave the tree empty"
    );
}

fn assert_positions_match(actual: [f64; 3], expected: [f64; 3], label: &str) {
    for axis in 0..3 {
        assert!(
            (actual[axis] - expected[axis]).abs() < 1e-9,
            "{label}: axis {axis} expected {}, got {}",
            expected[axis],
            actual[axis]
        );
    }
}
