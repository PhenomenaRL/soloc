//! Transformation Engine for projecting spacetimestamps across astronomical frames.
//!
//! This module provides the core transformation anywhere in the solar system, taking raw
//! `RecordBatch` data containing embedded `spacetimestamp`s and mathematically
//! projecting their coordinates into any target reference frame.
//!
//! It combines caller-supplied frame resolution (the ledger's derived transform tree) with
//! dynamic, time-varying orbital traversals (using `anise::Almanac`).
//!
//! # Transform Pipeline
//!
//! For each row, the full transform is a three-stage composition:
//!
//! ```text
//! 1. Resolve: P_local  --[caller's resolver ]--> P_root   (local/entity frame → astronomical root)
//! 2. Dynamic: P_root   --[Almanac.rotate()  ]--> P_rot    (re-orient into target frame axes)
//! 3. Dynamic: P_rot    --[Almanac.translate()]--> P_target (shift origin to target frame origin)
//! ```
//!
//! Positions use `Point3` (isometry applies translation + rotation).
//! Orientations use `UnitQuaternion` (rotation only).
//! Kinematic fields outside the spacetimestamp struct (velocity, angular_velocity, acceleration)
//! are passed through unchanged; the caller is responsible for reprojecting those.
//!
//! # Anise Convention
//!
//! `almanac.translate(from_frame, to_frame, epoch, ...)` returns a `CartesianState` whose
//! `radius_km` is the position of the `from_frame` origin expressed in `to_frame` coordinates.
//! This is the additive offset applied after rotating the position vector into target orientation.
//!
//! # Timescale Note
//!
//! `duration_centuries` + `duration_ns` are offsets from that row's `timescale_id` J2000
//! epoch, and [`epoch_from_parts`] applies the right reference for it, so a reconstructed
//! epoch is physically correct whatever the row was stored in. A ledger normalises everything
//! to TAI at append; a batch reaching this function directly need not be.
//!
//! # Unit Convention for Resolved Frames
//!
//! Isometries returned by the resolver are always in kilometres, regardless of the row's
//! `units_pos` field. Row coordinates are converted to km before the isometry is applied.

use anise::prelude::*;
use arrow::array::{Array, Int16Builder, StructArray, UInt8Builder, UInt64Builder};
use arrow::record_batch::RecordBatch;
use nalgebra::{Isometry3, Point3, Quaternion, Rotation3, UnitQuaternion, Vector3};
use std::collections::HashMap;
use std::sync::Arc;

use crate::ephemeris::{epoch_from_parts, epoch_to_parts};
use crate::identity::{IdMap, PrescribedId};
use crate::schema::{
    DURATION_CENTURIES_COLUMN, DURATION_NS_COLUMN, STS_COLUMN, SpaceTimestampBuilder, StsColumns,
    TIMESCALE_ID_COLUMN,
};
use crate::vocabulary::{LengthUnit, TimeScaleCode, Vocabulary};

/// A resolved dynamic frame: the astronomical root its chain terminates in, plus the
/// isometry (always in km) taking child-frame coordinates into that root.
pub type ResolvedFrame = (PrescribedId, Isometry3<f64>);

/// Resolves a [`KIND_SOLOC`](crate::identity::KIND_SOLOC) frame id at a given epoch: see
/// [`transform_batch`].
pub type DynamicFrameResolver<'a> = &'a dyn Fn(PrescribedId, Epoch) -> Option<ResolvedFrame>;

/// Resolves an astronomical root id to the `anise` [`Frame`] it embeds.
///
/// A KIND_ASTRO id carries its `(ephemeris_id, orientation_id)` pair, so the frame is read
/// straight out of the id with no name registry.
fn resolve_root_frame(id: PrescribedId, epoch: Epoch) -> Result<Frame, String> {
    if id.is_soloc() {
        return Err(format!(
            "Failed resolving frame {id}: it is an entity reference, but no pose for it \
             could be resolved at {epoch}"
        ));
    }
    let (ephemeris_id, orientation_id) = id.astro_frame().ok_or_else(|| {
        format!("Failed resolving frame {id}: abstract ids label provenance and are never frames")
    })?;
    Ok(Frame::new(ephemeris_id, orientation_id))
}

/// A human label for an astronomical root in an error message: its canonical name from the
/// embedded pair, or the hyphenated id if the pair is somehow unnamed.
fn root_display(id: PrescribedId) -> String {
    id.astro_frame()
        .and_then(|(e, o)| crate::ephemeris::frame_name(e, o))
        .map(String::from)
        .unwrap_or_else(|| id.to_hyphenated())
}

/// Reprojects the `spacetimestamp` struct column of a batch into `target_frame_name`.
///
/// A pure projection: the input is unmodified and only spacetimestamp struct column is
/// replaced in the result. Callers are responsible for transforming fields in parent
/// schemas (for now).
///
/// # Arguments
/// * `resolve_dynamic_frame`: called when a row's `frame_id` is a
///   [`KIND_SOLOC`](crate::identity::KIND_SOLOC) id needing a pose lookup. It receives
///   **that row's own epoch**, so a batch spanning several timesteps resolves each row
///   against the pose current at its own; answers are memoised per `(frame, epoch)`.
///   Its isometries are always km regardless of `units_pos`. A frame it declines to place
///   is treated as an astronomical root, whose `anise` frame is read straight from the id.
pub fn transform_batch(
    batch: &RecordBatch,
    target_frame_name: &str,
    almanac: &Almanac,
    target_unit: LengthUnit,
    resolve_dynamic_frame: Option<DynamicFrameResolver<'_>>,
) -> Result<RecordBatch, String> {
    // Attempt to resolve the target frame in anise. We default to assuming J2000 orientation
    // if the user simply passed a planetary center like "Mars".  Also handles "IAU_BODY" strings
    // (e.g. "IAU_EARTH") by mapping to the corresponding anise body-fixed frame.
    let target_frame =
        crate::ephemeris::resolve_astronomical_frame(target_frame_name).ok_or_else(|| {
            format!(
                "Invalid target_frame_name '{}': not recognized by anise",
                target_frame_name
            )
        })?;

    let target_frame_id =
        PrescribedId::astronomical(target_frame.ephemeris_id, target_frame.orientation_id)?;

    let schema = batch.schema();
    let col_idx = schema
        .index_of(STS_COLUMN)
        .map_err(|_| format!("Column '{}' not found", STS_COLUMN))?;

    let cols = StsColumns::try_new(batch)?;

    let num_rows = batch.num_rows();
    let mut sts_builder = SpaceTimestampBuilder::new(num_rows);

    // Resolver answers per (frame, epoch), so a per-timestep snapshot batch costs one call
    // rather than one per row. Deliberately the default hasher, not `IdMap`: the key is a
    // tuple, so `IdHasher` would fall back to FNV for the epoch half anyway.
    let mut dynamic_cache: HashMap<(PrescribedId, (i16, u64)), Option<ResolvedFrame>> =
        HashMap::new();

    // Registry lookup and anise name resolution per root id. A batch typically terminates
    // in one or two roots, so this replaces a per-row string resolve with a 16-byte hash.
    let mut root_cache: IdMap<Frame> = IdMap::default();

    for i in 0..num_rows {
        let original_frame_id = cols.frame_at(i)?;
        let source_id = cols.source_at(i)?;
        let current_unit = cols.units_at(i)?;
        let ts = cols.timescale_at(i)?;
        let est = cols.estimate_at(i)?;

        let [px, py, pz] = cols.position_at(i);
        let [qw, qx, qy, qz] = cols.quaternion_at(i);

        let (centuries, ns) = cols.epoch_parts_at(i);

        // Reconstruct the physical epoch from the stored (centuries, ns) and their declared
        // timescale. epoch_from_parts applies the correct J2000 reference for that timescale
        // so the resulting Epoch is always physically correct regardless of storage timescale.
        let epoch = epoch_from_parts(centuries, ns, ts.into());

        // --- Stage 1: Local frame → astronomical root ---
        //
        // Two possible resolutions, checked in priority order:
        //   a) Resolver placed the frame : isometry is in km (entity pose from ledger)
        //   b) Passthrough               : original_frame is already an astronomical root
        let quat_local = UnitQuaternion::from_quaternion(Quaternion::new(qw, qx, qy, qz));

        // Resolved against this row's own epoch, so a batch spanning several timesteps
        // gets the pose that was current at each one.
        let dynamic = resolve_dynamic_frame.and_then(|resolve| {
            *dynamic_cache
                .entry((original_frame_id, epoch.to_tai_duration().to_parts()))
                .or_insert_with(|| resolve(original_frame_id, epoch))
        });

        let (root_frame_id, pos_root, quat_root) = if let Some((root, iso)) = dynamic {
            // Resolved: isometry in km; normalize coordinates to km first.
            let pos_km = Point3::new(
                current_unit.to_km(px),
                current_unit.to_km(py),
                current_unit.to_km(pz),
            );
            let pos_root = (iso * pos_km).coords;
            let quat_root = iso.rotation * quat_local;
            (root, pos_root, quat_root)
        } else {
            // Passthrough: original_frame_id is an astronomical root already.
            let pos_root = Vector3::new(
                current_unit.to_km(px),
                current_unit.to_km(py),
                current_unit.to_km(pz),
            );
            (original_frame_id, pos_root, quat_local)
        };

        // --- Stage 2 & 3: Dynamic transform (astronomical root → target) via Almanac ---
        //
        // Root ids are always terminal astronomical identifiers: "ICRF", "IAU_EARTH",
        // "IAU_TITAN", etc. KIND_SOLOC ids are resolved before reaching this point and
        // never appear here as a root.
        let root_frame = match root_cache.get(&root_frame_id) {
            Some(frame) => *frame,
            None => {
                let frame = resolve_root_frame(root_frame_id, epoch)?;
                root_cache.insert(root_frame_id, frame);
                frame
            }
        };

        // almanac.translate(from, to, epoch) → radius_km is the position of `from`'s origin
        // expressed in `to` frame coordinates. This is the additive shift that maps a vector
        // already rotated into `to` orientation from the `from` origin to the `to` origin.
        let translation = almanac
            .translate(root_frame, target_frame, epoch, None)
            .map_err(|e| {
                let root = root_display(root_frame_id);
                format!("Anise translate error ('{root}' → '{target_frame_name}'): {e}")
            })?;
        let pos_root_wrt_target = translation.radius_km;

        // almanac.rotate(from, to) → DCM R such that v_target = R * v_root
        let dcm = almanac
            .rotate(root_frame, target_frame, epoch)
            .map_err(|e| {
                let root = root_display(root_frame_id);
                format!("Anise rotate error ('{root}' → '{target_frame_name}'): {e}")
            })?;
        let rot_matrix: Rotation3<f64> = Rotation3::from_matrix_unchecked(dcm.rot_mat);
        let rot_quat: UnitQuaternion<f64> = UnitQuaternion::from_rotation_matrix(&rot_matrix);

        // Compose final position and orientation (all quantities in km at this point)
        let pos_target = rot_matrix * pos_root + pos_root_wrt_target;
        let quat_target = rot_quat * quat_root;

        // Convert position back to the requested output unit
        let pos_out = pos_target.map(|v| target_unit.from_km(v));

        // TODO(covariance): transform_batch does not currently propagate position_covariance
        // or orientation_covariance. Transforming covariance requires applying the rotation
        // Jacobian: C' = R·C·Rᵀ. Until implemented, covariance is set to null in the output
        // to avoid silently producing covariance expressed in the wrong frame.
        sts_builder.append_spacetimestamp(
            target_frame_id,
            target_unit,
            ts,
            source_id,
            est,
            [pos_out.x, pos_out.y, pos_out.z],
            [quat_target.w, quat_target.i, quat_target.j, quat_target.k],
            centuries,
            ns,
            None, // position_covariance: see TODO above
            None, // orientation_covariance: see TODO above
        );
    }

    // Only the spacetimestamp struct column is replaced; every other column passes through,
    // and the caller reprojects those if needed.
    let mut final_columns = batch.columns().to_vec();
    final_columns[col_idx] = Arc::new(sts_builder.finish_as_struct());

    RecordBatch::try_new(schema, final_columns).map_err(|e| format!("Batch rebuild error: {e}"))
}

/// Rewrites the time fields in the named spacetimestamp struct column so every row is stored
/// relative to J2000 TAI, regardless of the original `timescale_id`.
///
/// After normalization, `timescale_id` is `TAI` for all rows and `(duration_centuries,
/// duration_ns)` are SI-second offsets from `2000-01-01T12:00:00 TAI`. Rows already in TAI
/// are passed through unchanged (fast path when all rows are TAI returns a cheap clone).
///
/// This is called by [`soloc::ledger::Ledger::append`] so that all stored data shares a
/// single timescale, making temporal comparisons and almanac queries unambiguous.
pub fn normalize_batch_to_tai(batch: &RecordBatch) -> Result<RecordBatch, String> {
    let schema = batch.schema();
    let col_idx = schema
        .index_of(STS_COLUMN)
        .map_err(|_| format!("Column '{}' not found in batch", STS_COLUMN))?;

    let struct_array = batch
        .column(col_idx)
        .as_any()
        .downcast_ref::<StructArray>()
        .ok_or_else(|| format!("'{}' is not a StructArray", STS_COLUMN))?;

    let cols = StsColumns::try_new(batch)?;

    // Fast path: every row is already TAI, nothing to do.
    let scales: Vec<TimeScaleCode> = (0..cols.len())
        .map(|i| cols.timescale_at(i))
        .collect::<Result<_, _>>()?;
    if scales.iter().all(|ts| *ts == TimeScaleCode::TAI) {
        return Ok(batch.clone());
    }

    let num_rows = batch.num_rows();
    let mut new_cents = Int16Builder::with_capacity(num_rows);
    let mut new_ns = UInt64Builder::with_capacity(num_rows);
    let mut new_ts = UInt8Builder::with_capacity(num_rows);

    for (i, ts) in scales.into_iter().enumerate() {
        let (centuries, ns) = cols.epoch_parts_at(i);
        let (c, n) = if ts == TimeScaleCode::TAI {
            (centuries, ns)
        } else {
            epoch_to_parts(epoch_from_parts(centuries, ns, ts.into()))
        };
        new_cents.append_value(c);
        new_ns.append_value(n);
        new_ts.append_value(TimeScaleCode::TAI.code());
    }

    // Rebuild the struct: replace only the three time-related child arrays.
    let struct_fields = struct_array.fields().clone();
    let mut new_children: Vec<Arc<dyn Array>> =
        struct_array.columns().iter().map(Arc::clone).collect();

    let ts_idx = struct_fields
        .iter()
        .position(|f| f.name() == TIMESCALE_ID_COLUMN)
        .unwrap();
    let cent_idx = struct_fields
        .iter()
        .position(|f| f.name() == DURATION_CENTURIES_COLUMN)
        .unwrap();
    let ns_idx = struct_fields
        .iter()
        .position(|f| f.name() == DURATION_NS_COLUMN)
        .unwrap();

    new_children[ts_idx] = Arc::new(new_ts.finish());
    new_children[cent_idx] = Arc::new(new_cents.finish());
    new_children[ns_idx] = Arc::new(new_ns.finish());

    let new_struct =
        StructArray::try_new(struct_fields, new_children, struct_array.nulls().cloned())
            .map_err(|e| format!("Failed to rebuild spacetimestamp struct: {e}"))?;

    let mut final_columns = batch.columns().to_vec();
    final_columns[col_idx] = Arc::new(new_struct);

    RecordBatch::try_new(schema, final_columns)
        .map_err(|e| format!("Failed to rebuild batch after TAI normalization: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::datatypes::{DataType, Field, Schema};
    use nalgebra::Translation3;

    use crate::schema::{SpaceTimestampBuilder, sts_schema};
    use crate::vocabulary::EstimateType;

    /// Build a one-column RecordBatch wrapping a SpaceTimestamp StructArray.
    fn make_sts_batch(builder: &mut SpaceTimestampBuilder) -> RecordBatch {
        let struct_array = builder.finish_as_struct();
        let sts_ref = sts_schema();
        let schema = Arc::new(Schema::new(vec![Field::new(
            "spacetimestamp",
            DataType::Struct(sts_ref.fields().clone()),
            false,
        )]));
        RecordBatch::try_new(schema, vec![Arc::new(struct_array)]).unwrap()
    }

    /// An entity frame, as a client would mint one.
    fn entity(name: &str) -> PrescribedId {
        PrescribedId::new("bot", name).unwrap()
    }

    fn source() -> PrescribedId {
        PrescribedId::abstract_source("test", "s").unwrap()
    }

    /// Builds a resolver that places each frame id at a fixed `(root, isometry)`,
    /// standing in for what `Ledger::resolve_to_root` supplies in production. Isometries
    /// are in km, per the resolver contract.
    fn fixed_resolver(
        placements: Vec<(PrescribedId, PrescribedId, Isometry3<f64>)>,
    ) -> impl Fn(PrescribedId, Epoch) -> Option<ResolvedFrame> {
        move |frame: PrescribedId, _epoch: Epoch| {
            placements
                .iter()
                .find(|(id, _, _)| *id == frame)
                .map(|(_, root, iso)| (*root, *iso))
        }
    }

    fn iso_km(translation: [f64; 3], quat_wxyz: [f64; 4]) -> Isometry3<f64> {
        Isometry3::from_parts(
            Translation3::new(translation[0], translation[1], translation[2]),
            UnitQuaternion::from_quaternion(Quaternion::new(
                quat_wxyz[0],
                quat_wxyz[1],
                quat_wxyz[2],
                quat_wxyz[3],
            )),
        )
    }

    fn read_output_pos(result: &RecordBatch, row: usize) -> [f64; 3] {
        StsColumns::try_new(result).unwrap().position_at(row)
    }

    fn read_output_quat(result: &RecordBatch, row: usize) -> [f64; 4] {
        StsColumns::try_new(result).unwrap().quaternion_at(row)
    }

    /// A frame the resolver places one hop from its root. `bot:cam` sits at [1, 0, 0] km
    /// from Earth, so a point at the cam origin should appear at [1, 0, 0] km in Earth frame.
    #[test]
    fn test_resolved_single_hop_translation() {
        let (cam, earth) = (
            entity("cam"),
            PrescribedId::astronomical_from_name("Earth").unwrap(),
        );

        let mut builder = SpaceTimestampBuilder::new(1);
        builder.append_spacetimestamp(
            cam,
            LengthUnit::km,
            TimeScaleCode::TAI,
            source(),
            EstimateType::MEASURED,
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            0,
            None,
            None,
        );

        let batch = make_sts_batch(&mut builder);
        let resolver = fixed_resolver(vec![(
            cam,
            earth,
            iso_km([1.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0]),
        )]);
        let result = transform_batch(
            &batch,
            "Earth",
            &Almanac::default(),
            LengthUnit::km,
            Some(&resolver),
        )
        .unwrap();

        let pos = read_output_pos(&result, 0);
        assert_eq!(pos, [1.0, 0.0, 0.0]);
    }

    /// A multi-hop chain arrives here already composed. The ledger walks cam → base_link →
    /// Earth and hands back a single isometry. base_link is at [0, 1, 0] km from Earth and cam
    /// is at [1, 0, 0] km from base_link, so the composed offset is [1, 1, 0] km.
    #[test]
    fn test_resolved_composed_chain() {
        let (cam, earth) = (
            entity("cam"),
            PrescribedId::astronomical_from_name("Earth").unwrap(),
        );

        let mut builder = SpaceTimestampBuilder::new(1);
        builder.append_spacetimestamp(
            cam,
            LengthUnit::km,
            TimeScaleCode::TAI,
            source(),
            EstimateType::MEASURED,
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            0,
            None,
            None,
        );

        let batch = make_sts_batch(&mut builder);
        let resolver = fixed_resolver(vec![(
            cam,
            earth,
            iso_km([1.0, 1.0, 0.0], [1.0, 0.0, 0.0, 0.0]),
        )]);
        let result = transform_batch(
            &batch,
            "Earth",
            &Almanac::default(),
            LengthUnit::km,
            Some(&resolver),
        )
        .unwrap();

        let pos = read_output_pos(&result, 0);
        assert!((pos[0] - 1.0).abs() < 1e-10, "x={}", pos[0]);
        assert!((pos[1] - 1.0).abs() < 1e-10, "y={}", pos[1]);
        assert!(pos[2].abs() < 1e-10, "z={}", pos[2]);
    }

    /// Rotation-only placement: cam is rotated 90° about Z relative to Earth (no translation).
    /// A point along +X in cam frame should appear along +Y in Earth frame, and the output
    /// quaternion should carry the same 90°-Z rotation.
    #[test]
    fn test_resolved_rotation_only() {
        use std::f64::consts::FRAC_PI_2;
        // 90° rotation about Z: quaternion = [cos(45°), 0, 0, sin(45°)] = [√2/2, 0, 0, √2/2]
        let half = FRAC_PI_2 / 2.0;
        let (cos_h, sin_h) = (half.cos(), half.sin());

        let (cam, earth) = (
            entity("cam"),
            PrescribedId::astronomical_from_name("Earth").unwrap(),
        );

        let mut builder = SpaceTimestampBuilder::new(1);
        // Point at [1, 0, 0] in cam frame; after 90°-Z rotation it should be at [0, 1, 0] in Earth.
        builder.append_spacetimestamp(
            cam,
            LengthUnit::km,
            TimeScaleCode::TAI,
            source(),
            EstimateType::MEASURED,
            [1.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0], // identity orientation of the sensor itself
            0,
            0,
            None,
            None,
        );

        let batch = make_sts_batch(&mut builder);
        let resolver = fixed_resolver(vec![(
            cam,
            earth,
            iso_km([0.0, 0.0, 0.0], [cos_h, 0.0, 0.0, sin_h]),
        )]);
        let result = transform_batch(
            &batch,
            "Earth",
            &Almanac::default(),
            LengthUnit::km,
            Some(&resolver),
        )
        .unwrap();

        let pos = read_output_pos(&result, 0);
        assert!(pos[0].abs() < 1e-10, "x should be ~0, got {}", pos[0]);
        assert!(
            (pos[1] - 1.0).abs() < 1e-10,
            "y should be ~1, got {}",
            pos[1]
        );
        assert!(pos[2].abs() < 1e-10, "z should be ~0, got {}", pos[2]);

        // The output quaternion should be the sensor orientation composed with the frame rotation.
        // identity sensor * 90°-Z frame = 90°-Z.
        let [qw, qx, qy, qz] = read_output_quat(&result, 0);
        assert!((qw - cos_h).abs() < 1e-10, "qw={qw}");
        assert!(qx.abs() < 1e-10, "qx={qx}");
        assert!(qy.abs() < 1e-10, "qy={qy}");
        assert!((qz - sin_h).abs() < 1e-10, "qz={qz}");
    }

    /// Mixed batch: row 0 is already in Earth frame (resolver declines it, so it passes through),
    /// row 1 is in a resolved frame at [2, 0, 0] km from Earth. Both transform in one call.
    #[test]
    fn test_mixed_frame_batch() {
        let (arm, earth) = (
            entity("arm"),
            PrescribedId::astronomical_from_name("Earth").unwrap(),
        );

        let mut builder = SpaceTimestampBuilder::new(2);
        // Row 0: already in Earth frame at position [5, 0, 0]
        builder.append_spacetimestamp(
            earth,
            LengthUnit::km,
            TimeScaleCode::TAI,
            source(),
            EstimateType::MEASURED,
            [5.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            0,
            None,
            None,
        );
        // Row 1: in arm frame at [0, 0, 0] → should become [2, 0, 0] in Earth
        builder.append_spacetimestamp(
            arm,
            LengthUnit::km,
            TimeScaleCode::TAI,
            source(),
            EstimateType::MEASURED,
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            0,
            None,
            None,
        );

        let batch = make_sts_batch(&mut builder);
        let resolver = fixed_resolver(vec![(
            arm,
            earth,
            iso_km([2.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0]),
        )]);
        let result = transform_batch(
            &batch,
            "Earth",
            &Almanac::default(),
            LengthUnit::km,
            Some(&resolver),
        )
        .unwrap();

        let pos0 = read_output_pos(&result, 0);
        assert!((pos0[0] - 5.0).abs() < 1e-10, "row0 x={}", pos0[0]);

        let pos1 = read_output_pos(&result, 1);
        assert!((pos1[0] - 2.0).abs() < 1e-10, "row1 x={}", pos1[0]);
    }

    /// Unit conversion: a point at [1000, 0, 0] m in a frame with no offset
    /// should appear at [1.0, 0, 0] km after transforming with target_unit = km.
    #[test]
    fn test_unit_conversion_m_to_km() {
        let mut builder = SpaceTimestampBuilder::new(1);
        builder.append_spacetimestamp(
            PrescribedId::astronomical_from_name("Earth").unwrap(),
            LengthUnit::m,
            TimeScaleCode::TAI,
            source(),
            EstimateType::MEASURED,
            [1000.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            0,
            None,
            None,
        );

        let batch = make_sts_batch(&mut builder);
        let result =
            transform_batch(&batch, "Earth", &Almanac::default(), LengthUnit::km, None).unwrap();

        let pos = read_output_pos(&result, 0);
        assert!(
            (pos[0] - 1.0).abs() < 1e-10,
            "expected 1.0 km, got {}",
            pos[0]
        );
    }

    // -----------------------------------------------------------------------
    // FRICTION 8: IAU body-fixed frame name resolution
    // -----------------------------------------------------------------------

    #[test]
    fn test_iau_resolve_planets() {
        let cases = [
            ("IAU_SUN", 10),
            ("IAU_MERCURY", 199),
            ("IAU_VENUS", 299),
            ("IAU_EARTH", 399),
            ("IAU_MOON", 301),
            ("IAU_MARS", 499),
            ("IAU_JUPITER", 599),
            ("IAU_SATURN", 699),
            ("IAU_URANUS", 799),
            ("IAU_NEPTUNE", 899),
        ];
        for (name, naif_id) in cases {
            let frame = crate::ephemeris::resolve_astronomical_frame(name)
                .unwrap_or_else(|| panic!("resolve_astronomical_frame({name:?}) returned None"));
            assert_eq!(frame.ephemeris_id, naif_id, "{name}: wrong ephemeris_id");
            assert_eq!(
                frame.orientation_id, naif_id,
                "{name}: wrong orientation_id"
            );
        }
    }

    #[test]
    fn test_iau_resolve_major_moons() {
        let cases = [
            ("IAU_TITAN", 606),
            ("IAU_EUROPA", 502),
            ("IAU_GANYMEDE", 503),
            ("IAU_CALLISTO", 504),
            ("IAU_IO", 501),
            ("IAU_TRITON", 801),
            ("IAU_PHOBOS", 401),
            ("IAU_DEIMOS", 402),
        ];
        for (name, naif_id) in cases {
            let frame = crate::ephemeris::resolve_astronomical_frame(name)
                .unwrap_or_else(|| panic!("resolve_astronomical_frame({name:?}) returned None"));
            assert_eq!(frame.ephemeris_id, naif_id, "{name}: wrong ephemeris_id");
        }
    }

    #[test]
    fn test_iau_resolve_unknown() {
        assert!(crate::ephemeris::resolve_astronomical_frame("IAU_UNKNOWN").is_none());
        assert!(crate::ephemeris::resolve_astronomical_frame("IAU_").is_none());
        assert!(
            crate::ephemeris::resolve_astronomical_frame("IAU_earth").is_none(),
            "must be uppercase"
        );
    }

    // -----------------------------------------------------------------------
    // normalize_batch_to_tai tests
    // -----------------------------------------------------------------------

    fn read_timescale(batch: &RecordBatch, row: usize) -> TimeScaleCode {
        StsColumns::try_new(batch)
            .unwrap()
            .timescale_at(row)
            .unwrap()
    }

    fn read_centuries_ns(batch: &RecordBatch, row: usize) -> (i16, u64) {
        StsColumns::try_new(batch).unwrap().epoch_parts_at(row)
    }

    #[test]
    fn test_normalize_tai_is_noop() {
        let mut builder = SpaceTimestampBuilder::new(2);
        builder.append_spacetimestamp(
            PrescribedId::astronomical_from_name("ICRF").unwrap(),
            LengthUnit::km,
            TimeScaleCode::TAI,
            source(),
            EstimateType::MEASURED,
            [0.0; 3],
            [1.0, 0.0, 0.0, 0.0],
            0,
            1000,
            None,
            None,
        );
        builder.append_spacetimestamp(
            PrescribedId::astronomical_from_name("ICRF").unwrap(),
            LengthUnit::km,
            TimeScaleCode::TAI,
            source(),
            EstimateType::MEASURED,
            [0.0; 3],
            [1.0, 0.0, 0.0, 0.0],
            0,
            2000,
            None,
            None,
        );
        let batch = make_sts_batch(&mut builder);
        let result = normalize_batch_to_tai(&batch).unwrap();
        // Should be a cheap clone — same pointer
        assert_eq!(result.num_rows(), 2);
        assert_eq!(read_timescale(&result, 0), TimeScaleCode::TAI);
        assert_eq!(read_centuries_ns(&result, 0), (0, 1000));
    }

    #[test]
    fn test_normalize_utc_to_tai_shifts_by_leap_seconds() {
        use crate::ephemeris::{epoch_from_parts, epoch_to_parts, j2000_in_timescale};
        // Build a batch with timescale_id = "UTC" and parts relative to J2000 UTC.
        let j2000_utc = j2000_in_timescale(TimeScale::UTC);
        let offset = hifitime::Duration::from_parts(0, 5_000_000_000u64); // 5 seconds
        let utc_epoch = j2000_utc + offset;
        let (utc_c, utc_n) = (utc_epoch - j2000_utc).to_parts();

        let mut builder = SpaceTimestampBuilder::new(1);
        builder.append_spacetimestamp(
            PrescribedId::astronomical_from_name("ICRF").unwrap(),
            LengthUnit::km,
            TimeScaleCode::UTC,
            source(),
            EstimateType::MEASURED,
            [0.0; 3],
            [1.0, 0.0, 0.0, 0.0],
            utc_c,
            utc_n,
            None,
            None,
        );
        let batch = make_sts_batch(&mut builder);

        let result = normalize_batch_to_tai(&batch).unwrap();

        // After normalization: timescale_id should be TAI.
        assert_eq!(read_timescale(&result, 0), TimeScaleCode::TAI);

        // The stored TAI parts should represent the same physical moment.
        let (tai_c, tai_n) = read_centuries_ns(&result, 0);
        let recovered = epoch_from_parts(tai_c, tai_n, TimeScale::TAI);
        assert_eq!(
            recovered, utc_epoch,
            "normalized TAI epoch should equal original UTC epoch"
        );

        // The TAI parts differ from the UTC parts (TAI J2000 ≠ UTC J2000).
        let expected_tai_parts = epoch_to_parts(utc_epoch);
        assert_eq!((tai_c, tai_n), expected_tai_parts);
    }

    #[test]
    fn test_normalize_mixed_timescales() {
        use crate::ephemeris::{epoch_from_parts, j2000_in_timescale};
        // Row 0: TAI, should be unchanged.
        // Row 1: UTC, should be converted.
        let j2000_utc = j2000_in_timescale(TimeScale::UTC);
        let utc_epoch = j2000_utc + hifitime::Duration::from_parts(0, 10_000_000_000u64);
        let (utc_c, utc_n) = (utc_epoch - j2000_utc).to_parts();

        let mut builder = SpaceTimestampBuilder::new(2);
        builder.append_spacetimestamp(
            PrescribedId::astronomical_from_name("ICRF").unwrap(),
            LengthUnit::km,
            TimeScaleCode::TAI,
            source(),
            EstimateType::MEASURED,
            [0.0; 3],
            [1.0, 0.0, 0.0, 0.0],
            0,
            500,
            None,
            None,
        );
        builder.append_spacetimestamp(
            PrescribedId::astronomical_from_name("ICRF").unwrap(),
            LengthUnit::km,
            TimeScaleCode::UTC,
            source(),
            EstimateType::MEASURED,
            [0.0; 3],
            [1.0, 0.0, 0.0, 0.0],
            utc_c,
            utc_n,
            None,
            None,
        );
        let batch = make_sts_batch(&mut builder);

        let result = normalize_batch_to_tai(&batch).unwrap();

        assert_eq!(read_timescale(&result, 0), TimeScaleCode::TAI);
        assert_eq!(read_timescale(&result, 1), TimeScaleCode::TAI);
        assert_eq!(read_centuries_ns(&result, 0), (0, 500));
        let (c1, n1) = read_centuries_ns(&result, 1);
        assert_eq!(epoch_from_parts(c1, n1, TimeScale::TAI), utc_epoch);
    }
}
