//! Transformation Engine for projecting spacetimestamps into any frame of the topology.
//!
//! This module provides the core transformation anywhere in the solar system, taking raw
//! `RecordBatch` data containing embedded `spacetimestamp`s and mathematically
//! projecting their coordinates into a target reference frame: either an astronomical frame
//! or any entity whose pose chain the caller can resolve.
//!
//! It combines caller-supplied frame resolution (the ledger's derived transform tree) with
//! dynamic, time-varying orbital traversals (using `anise::Almanac`).
//!
//! # Transform Pipeline
//!
//! For each row, the full transform is a four-stage composition:
//!
//! ```text
//! 1. Resolve: P_local  --[caller's resolver  ]--> P_rootA  (row frame → its astronomical root)
//! 2. Dynamic: P_rootA  --[Almanac.rotate()   ]--> P_rot    (re-orient into rootB's axes)
//! 3. Dynamic: P_rot    --[Almanac.translate()]--> P_rootB  (shift origin to rootB's origin)
//! 4. Resolve: P_rootB  --[target chain⁻¹     ]--> P_target (rootB → target entity frame)
//! ```
//!
//! In full: `T_target←src = (T_rootB←target)⁻¹ · T_almanac(rootA→rootB, t) · T_rootA←src`.
//!
//! The target entity is not moved in the topology: it stays a child in its own tree, rooted
//! at an astronomical frame, and the output simply expresses each row relative to it. An
//! astronomical target is its own root, so stage 4 is the identity. When both chains end in
//! the same root, stages 2 and 3 are skipped entirely: the transform is purely local, needs
//! no almanac data, and never routes through the root's (possibly very large) coordinates.
//!
//! Positions use `Point3` (isometry applies translation + rotation).
//! Orientations use `UnitQuaternion` (rotation only).
//! Kinematic fields outside the spacetimestamp struct (velocity, angular_velocity, acceleration)
//! are passed through unchanged; the caller is responsible for reprojecting those. The output
//! is the **geometric** state: no light-time or aberration correction is applied.
//!
//! # Estimate Type
//!
//! A transformed row is a derived value: it folds in every pose on both chains plus the
//! ephemeris, and its covariance is dropped. Its `estimate_type` is the worst of the source
//! row's and every composed pose's (see [`EstimateType::worst`]), then
//! [`derived`](EstimateType::derived): SIMULATED stays SIMULATED, everything else becomes
//! ESTIMATED. A row already expressed in the target frame is not transformed and keeps its
//! estimate type; only its units are converted.
//!
//! # Anise Convention
//!
//! `almanac.translate(from_frame, to_frame, epoch, ...)` returns a `CartesianState` whose
//! `radius_km` is the position of the `from_frame` origin relative to the `to_frame` origin,
//! expressed in **J2000 axes** whatever either frame's orientation: anise does not rotate SPK
//! data. It must be rotated J2000 → target before it is added. Skipping that rotation is
//! exact only for a J2000-aligned target (ICRF, GCRF), and is off by up to the origin
//! distance otherwise. anise's own `Almanac::transform` skips it too; `transform_to` does not.
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

use anise::constants::orientations::J2000;
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
use crate::vocabulary::{EstimateType, LengthUnit, TimeScaleCode, Vocabulary};

/// A frame resolved to the root its pose chain terminates in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResolvedFrame {
    /// The terminal frame of the chain: an astronomical frame or body handed to `anise`.
    pub root: PrescribedId,
    /// Takes coordinates in the resolved frame into `root`. Always kilometres.
    pub iso_km: Isometry3<f64>,
    /// The worst [`EstimateType`] of any pose composed into `iso_km`, by
    /// [`EstimateType::worst`]. MEASURED is the neutral value for a chain with no poses.
    pub estimate: EstimateType,
}

impl ResolvedFrame {
    /// Bundles a resolved chain.
    pub fn new(root: PrescribedId, iso_km: Isometry3<f64>, estimate: EstimateType) -> Self {
        Self {
            root,
            iso_km,
            estimate,
        }
    }

    /// The trivial resolution of an astronomical frame: itself, with no poses composed.
    fn identity(root: PrescribedId) -> Self {
        Self::new(root, Isometry3::identity(), EstimateType::MEASURED)
    }
}

/// Resolves a [`KIND_SOLOC`](crate::identity::KIND_SOLOC) frame id at a given epoch to its
/// chain root: see [`transform_batch_to`].
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

/// [`resolve_root_frame`], memoised per root id. A batch typically terminates in one or two
/// roots, so this replaces a per-row resolve with a 16-byte hash.
fn root_frame_cached(
    cache: &mut IdMap<Frame>,
    id: PrescribedId,
    epoch: Epoch,
) -> Result<Frame, String> {
    if let Some(frame) = cache.get(&id) {
        return Ok(*frame);
    }
    let frame = resolve_root_frame(id, epoch)?;
    cache.insert(id, frame);
    Ok(frame)
}

/// A human label for a frame in an error message: its canonical name from the embedded
/// astronomical pair, or the hyphenated id for anything else.
fn root_display(id: PrescribedId) -> String {
    id.astro_frame()
        .and_then(|(e, o)| crate::ephemeris::frame_name(e, o))
        .map(String::from)
        .unwrap_or_else(|| id.to_hyphenated())
}

/// Resolver answers per (frame, epoch). Deliberately the default hasher, not `IdMap`: the key
/// is a tuple, so `IdHasher` would fall back to FNV for the epoch half anyway.
type ResolverCache = HashMap<(PrescribedId, (i16, u64)), Option<ResolvedFrame>>;

/// Calls `resolve` for `frame` at `epoch`, memoised so a per-timestep snapshot batch costs
/// one call per frame rather than one per row.
fn resolve_cached(
    cache: &mut ResolverCache,
    resolve: DynamicFrameResolver<'_>,
    frame: PrescribedId,
    epoch: Epoch,
    epoch_key: (i16, u64),
) -> Option<ResolvedFrame> {
    *cache
        .entry((frame, epoch_key))
        .or_insert_with(|| resolve(frame, epoch))
}

/// Reprojects the `spacetimestamp` struct column of a batch into the astronomical frame
/// named `target_frame_name`.
///
/// Name-based convenience over [`transform_batch_to`], which also accepts entity targets.
/// Errors if the name is not a recognised astronomical frame.
pub fn transform_batch(
    batch: &RecordBatch,
    target_frame_name: &str,
    almanac: &Almanac,
    target_unit: LengthUnit,
    resolve_dynamic_frame: Option<DynamicFrameResolver<'_>>,
) -> Result<RecordBatch, String> {
    // Also handles "IAU_BODY" strings (e.g. "IAU_EARTH") by mapping to the corresponding anise
    // body-fixed frame.
    let target_frame =
        crate::ephemeris::resolve_astronomical_frame(target_frame_name).ok_or_else(|| {
            format!(
                "Invalid target_frame_name '{}': not recognized by anise",
                target_frame_name
            )
        })?;
    let target =
        PrescribedId::astronomical(target_frame.ephemeris_id, target_frame.orientation_id)?;
    transform_batch_to(batch, target, almanac, target_unit, resolve_dynamic_frame)
}

/// Reprojects the `spacetimestamp` struct column of a batch into `target`: an astronomical
/// frame, or any [`KIND_SOLOC`](crate::identity::KIND_SOLOC) entity the resolver can place.
///
/// A pure projection: the input is unmodified and only the spacetimestamp struct column is
/// replaced in the result. Callers are responsible for transforming fields in parent
/// schemas (for now). Every output row has `frame_id = target`, `units_pos = target_unit`,
/// and an estimate type set as described in the [module docs](self#estimate-type).
///
/// # Arguments
/// * `resolve_dynamic_frame`: called for a row's `frame_id` that is a
///   [`KIND_SOLOC`](crate::identity::KIND_SOLOC) id needing a pose lookup, and for an entity
///   `target`. It receives **that row's own epoch**, so a batch spanning several timesteps
///   resolves each row, and the target's possibly moving pose, against the pose current at
///   that epoch; answers are memoised per `(frame, epoch)`. Its isometries are always km
///   regardless of `units_pos`. A row frame it declines to place is treated as a root, whose
///   `anise` frame is read straight from the id.
///
/// # Errors
/// The whole batch fails if `target` is an abstract id, if `target` is an entity and either
/// no resolver is given or it cannot place `target` at some row's epoch, or if any row's
/// chain cannot be carried to the target's root.
pub fn transform_batch_to(
    batch: &RecordBatch,
    target: PrescribedId,
    almanac: &Almanac,
    target_unit: LengthUnit,
    resolve_dynamic_frame: Option<DynamicFrameResolver<'_>>,
) -> Result<RecordBatch, String> {
    if target.is_abstract() {
        return Err(format!(
            "Invalid target frame {target}: abstract ids label provenance and are never frames"
        ));
    }
    // An astronomical target is its own root, fixed for the whole call. An entity target is
    // placed per epoch below, since it may move.
    let fixed_target = if target.is_soloc() {
        if resolve_dynamic_frame.is_none() {
            return Err(format!(
                "Target frame {} is an entity, which needs a frame resolver to place it",
                root_display(target)
            ));
        }
        None
    } else {
        Some(ResolvedFrame::identity(target))
    };

    let schema = batch.schema();
    let col_idx = schema
        .index_of(STS_COLUMN)
        .map_err(|_| format!("Column '{}' not found", STS_COLUMN))?;

    let cols = StsColumns::try_new(batch)?;

    let num_rows = batch.num_rows();
    let mut sts_builder = SpaceTimestampBuilder::new(num_rows);

    // Shared by row frames and an entity target, so a target that also appears as a row
    // frame is resolved once per epoch.
    let mut dynamic_cache: ResolverCache = HashMap::new();

    let mut root_cache: IdMap<Frame> = IdMap::default();

    // J2000 → rootB rotation per (rootB, epoch). With an entity target, rootB can differ by
    // row (the target may be re-parented mid-batch), so it is part of the key.
    let mut j2000_cache: HashMap<(PrescribedId, (i16, u64)), Rotation3<f64>> = HashMap::new();

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
        let epoch_key = epoch.to_tai_duration().to_parts();

        // Every isometry below is in km, so coordinates are normalised to km up front.
        let pos_km = Vector3::new(
            current_unit.to_km(px),
            current_unit.to_km(py),
            current_unit.to_km(pz),
        );
        let quat_local = UnitQuaternion::from_quaternion(Quaternion::new(qw, qx, qy, qz));

        let (pos_target, quat_target, est_out) = if original_frame_id == target {
            // Already in the target frame: nothing is composed, so nothing is derived. Only
            // the units change, and the estimate type is the row's own.
            (pos_km, quat_local, est)
        } else {
            // --- Stage 1: row frame → its root (rootA) ---
            //
            // Two possible resolutions, checked in priority order:
            //   a) Resolver placed the frame: isometry in km (entity pose chain)
            //   b) Passthrough: original_frame_id is a root already
            let source = resolve_dynamic_frame
                .and_then(|resolve| {
                    resolve_cached(
                        &mut dynamic_cache,
                        resolve,
                        original_frame_id,
                        epoch,
                        epoch_key,
                    )
                })
                .unwrap_or_else(|| ResolvedFrame::identity(original_frame_id));
            let pos_a = (source.iso_km * Point3::from(pos_km)).coords;
            let quat_a = source.iso_km.rotation * quat_local;

            // --- Target placement: target → its root (rootB) ---
            let target_chain = match fixed_target {
                Some(fixed) => fixed,
                None => {
                    // Checked non-None above whenever the target is an entity.
                    let resolve = resolve_dynamic_frame.ok_or_else(|| {
                        format!("No frame resolver for target {}", root_display(target))
                    })?;
                    resolve_cached(&mut dynamic_cache, resolve, target, epoch, epoch_key)
                        .ok_or_else(|| {
                            format!(
                                "Target frame {} could not be resolved at {epoch}: no pose \
                                 chain to an astronomical root",
                                root_display(target)
                            )
                        })?
                }
            };

            // --- Stages 2 & 3: rootA → rootB via Almanac ---
            //
            // A shared root means the whole transform is local to one tree: the almanac
            // would contribute an exact identity, so it is not consulted at all.
            let (pos_b, quat_b) = if source.root == target_chain.root {
                (pos_a, quat_a)
            } else {
                let frame_a = root_frame_cached(&mut root_cache, source.root, epoch)?;
                let frame_b = root_frame_cached(&mut root_cache, target_chain.root, epoch)?;
                let anise_err = |op: &str, e: &dyn std::fmt::Display| {
                    format!(
                        "Anise {op} error ('{}' → '{}'): {e}",
                        root_display(source.root),
                        root_display(target_chain.root)
                    )
                };

                // almanac.translate(from, to, epoch) → radius_km is the position of `from`'s
                // origin relative to `to`'s origin, in J2000 axes whatever either frame's
                // orientation (the raw SPK data, unrotated).
                let translation = almanac
                    .translate(frame_a, frame_b, epoch, None)
                    .map_err(|e| anise_err("translate", &e))?;
                let pos_a_wrt_b = translation.radius_km;

                // almanac.rotate(from, to) → DCM R such that v_b = R * v_a
                let dcm = almanac
                    .rotate(frame_a, frame_b, epoch)
                    .map_err(|e| anise_err("rotate", &e))?;
                let rot_matrix: Rotation3<f64> = Rotation3::from_matrix_unchecked(dcm.rot_mat);
                let rot_quat: UnitQuaternion<f64> =
                    UnitQuaternion::from_rotation_matrix(&rot_matrix);

                // The translation is in J2000 axes, so it needs its own rotation into rootB's:
                // identity for a J2000-aligned rootB, unneeded when the origins coincide (the
                // translation is zero, and an almanac without orientation data still works),
                // else one lookup per (rootB, epoch).
                let j2000_to_b =
                    if frame_b.orientation_id == J2000 || frame_a.ephem_origin_match(frame_b) {
                        Rotation3::identity()
                    } else {
                        match j2000_cache.get(&(target_chain.root, epoch_key)) {
                            Some(rot) => *rot,
                            None => {
                                let dcm = almanac
                                    .rotate(frame_b.with_orient(J2000), frame_b, epoch)
                                    .map_err(|e| {
                                        format!(
                                            "Anise rotate error ('J2000' → '{}'): {e}",
                                            root_display(target_chain.root)
                                        )
                                    })?;
                                let rot = Rotation3::from_matrix_unchecked(dcm.rot_mat);
                                j2000_cache.insert((target_chain.root, epoch_key), rot);
                                rot
                            }
                        }
                    };

                (
                    rot_matrix * pos_a + j2000_to_b * pos_a_wrt_b,
                    rot_quat * quat_a,
                )
            };

            // --- Stage 4: rootB → target, by the inverse of the target's own chain ---
            // The identity for an astronomical target.
            let to_target = target_chain.iso_km.inverse();
            let pos_t = (to_target * Point3::from(pos_b)).coords;
            let quat_t = to_target.rotation * quat_b;

            let est_out = est
                .worst(source.estimate)
                .worst(target_chain.estimate)
                .derived();
            (pos_t, quat_t, est_out)
        };

        // Convert position back to the requested output unit
        let pos_out = pos_target.map(|v| target_unit.from_km(v));

        // TODO(covariance): transform_batch_to does not currently propagate
        // position_covariance or orientation_covariance. Transforming covariance requires
        // applying the rotation Jacobian: C' = R·C·Rᵀ, plus each composed pose's covariance.
        // Until implemented, covariance is set to null in the output to avoid silently
        // producing covariance expressed in the wrong frame.
        sts_builder.append_spacetimestamp(
            target,
            target_unit,
            ts,
            source_id,
            est_out,
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
    // The field takes the type of the struct actually built, which can differ from the
    // input's in field metadata alone (pyarrow adds an empty `ARROW:extension:metadata`).
    let sts = sts_builder.finish_as_struct();
    let mut fields = schema.fields().to_vec();
    fields[col_idx] = Arc::new(
        fields[col_idx]
            .as_ref()
            .clone()
            .with_data_type(sts.data_type().clone()),
    );
    let schema = Arc::new(arrow::datatypes::Schema::new_with_metadata(
        fields,
        schema.metadata().clone(),
    ));

    let mut final_columns = batch.columns().to_vec();
    final_columns[col_idx] = Arc::new(sts);

    RecordBatch::try_new(schema, final_columns).map_err(|e| format!("Batch rebuild error: {e}"))
}

/// Rewrites the time fields in the named spacetimestamp struct column so every row is stored
/// relative to J2000 TAI, regardless of the original `timescale_id`.
///
/// After normalization, `timescale_id` is `TAI` for all rows and `(duration_centuries,
/// duration_ns)` are SI-second offsets from `2000-01-01T12:00:00 TAI`. Rows already in TAI
/// are passed through unchanged (fast path when all rows are TAI returns a cheap clone).
///
/// This is called by `soloc_ledger::ledger::Ledger::append` so that all stored data shares a
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
    use anise::math::cartesian::CartesianState;
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
                .map(|(_, root, iso)| ResolvedFrame::new(*root, *iso, EstimateType::MEASURED))
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

    /// A batch whose struct fields differ from the canonical ones only in metadata, as pyarrow
    /// writes `arrow.uuid` fields, must still transform.
    #[test]
    fn test_transform_accepts_struct_fields_with_extra_metadata() {
        let mut builder = SpaceTimestampBuilder::new(1);
        builder.append_spacetimestamp(
            PrescribedId::astronomical_from_name("Earth").unwrap(),
            LengthUnit::km,
            TimeScaleCode::TAI,
            source(),
            EstimateType::MEASURED,
            [1.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            0,
            None,
            None,
        );
        let canonical = builder.finish_as_struct();
        let fields: Vec<Field> = canonical
            .fields()
            .iter()
            .map(|f| {
                let mut metadata = f.metadata().clone();
                if !metadata.contains_key("ARROW:extension:metadata") {
                    metadata.insert("ARROW:extension:metadata", "");
                }
                f.as_ref().clone().with_metadata(metadata)
            })
            .collect();
        let sts = StructArray::try_new(fields.into(), canonical.columns().to_vec(), None).unwrap();
        let schema = Arc::new(Schema::new(vec![Field::new(
            "spacetimestamp",
            sts.data_type().clone(),
            false,
        )]));
        let batch = RecordBatch::try_new(schema, vec![Arc::new(sts)]).unwrap();

        let result = transform_batch(&batch, "Earth", &Almanac::default(), LengthUnit::km, None)
            .expect("metadata alone must not fail the rebuild");
        assert_eq!(read_output_pos(&result, 0), [1.0, 0.0, 0.0]);
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

    // -----------------------------------------------------------------------
    // Root → target against real kernels (T1)
    // -----------------------------------------------------------------------

    /// Transforms one row at `pos_km` in `root` into `target` and checks it against anise's
    /// `transform_to` on the same state. `transform_to` is the oracle because it rotates into
    /// J2000 before translating; anise's `transform(root, target)` does not, and shares T1.
    fn assert_matches_anise(almanac: &Almanac, root: &str, target: &str, pos_km: [f64; 3]) {
        let epoch = Epoch::from_gregorian_tai_at_midnight(2026, 1, 1);
        let root_frame = crate::ephemeris::resolve_astronomical_frame(root).unwrap();
        let target_frame = crate::ephemeris::resolve_astronomical_frame(target).unwrap();
        let (centuries, ns) = epoch_to_parts(epoch);

        let mut builder = SpaceTimestampBuilder::new(1);
        builder.append_spacetimestamp(
            PrescribedId::astronomical_from_name(root).unwrap(),
            LengthUnit::km,
            TimeScaleCode::TAI,
            source(),
            EstimateType::MEASURED,
            pos_km,
            [1.0, 0.0, 0.0, 0.0],
            centuries,
            ns,
            None,
            None,
        );
        let batch = make_sts_batch(&mut builder);
        let result = transform_batch(&batch, target, almanac, LengthUnit::km, None).unwrap();
        let got = read_output_pos(&result, 0);

        let state = CartesianState::new(
            pos_km[0], pos_km[1], pos_km[2], 0.0, 0.0, 0.0, epoch, root_frame,
        );
        let expected = almanac
            .transform_to(state, target_frame, None)
            .unwrap()
            .radius_km;

        let err = (Vector3::from(got) - expected).norm();
        assert!(
            err < 1e-5,
            "{root} → {target}: got {got:?}, anise {expected:?}, off by {err} km"
        );
    }

    fn real_almanac() -> Almanac {
        MetaAlmanac::latest().expect("MetaAlmanac::latest() should succeed")
    }

    const P_KM: [f64; 3] = [7000.0, -1200.0, 300.0];

    // Rotating targets with a different origin: wrong before the T1 fix.

    #[test]
    #[ignore = "requires DE440s ephemeris (~150 MB download on first run, then cached)"]
    fn test_icrf_to_iau_sun_matches_anise() {
        assert_matches_anise(&real_almanac(), "ICRF", "IAU_SUN", P_KM);
    }

    #[test]
    #[ignore = "requires DE440s ephemeris (~150 MB download on first run, then cached)"]
    fn test_gcrf_to_iau_moon_matches_anise() {
        assert_matches_anise(&real_almanac(), "GCRF", "IAU_MOON", P_KM);
    }

    #[test]
    #[ignore = "requires DE440s ephemeris (~150 MB download on first run, then cached)"]
    fn test_iau_moon_to_iau_earth_matches_anise() {
        assert_matches_anise(&real_almanac(), "IAU_MOON", "IAU_EARTH", P_KM);
    }

    // Regressions: same origin, and inertial → inertial.

    #[test]
    #[ignore = "requires DE440s ephemeris (~150 MB download on first run, then cached)"]
    fn test_iau_earth_to_gcrf_matches_anise() {
        assert_matches_anise(&real_almanac(), "IAU_EARTH", "GCRF", P_KM);
    }

    #[test]
    #[ignore = "requires DE440s ephemeris (~150 MB download on first run, then cached)"]
    fn test_icrf_to_gcrf_matches_anise() {
        assert_matches_anise(&real_almanac(), "ICRF", "GCRF", P_KM);
    }
}
