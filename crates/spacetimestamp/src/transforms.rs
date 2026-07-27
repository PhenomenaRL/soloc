//! Transformation Engine for projecting spacetimestamps across astronomical frames.
//!
//! This module provides the core physics engine for `soloc`, taking raw
//! `RecordBatch` data containing embedded `spacetimestamp`s and mathematically
//! projecting their coordinates and velocities into any target reference frame.
//!
//! It combines static graph traversals (using the local `FrameRegistry`) with
//! dynamic, time-varying orbital traversals (using `anise::Almanac`).
//!
//! # Transform Pipeline
//!
//! For each row, the full transform is a three-stage composition:
//!
//! ```text
//! 1. Static:  P_local  --[FrameRegistry chain]--> P_root   (local sensor/robot frame → astronomical root)
//! 2. Dynamic: P_root   --[Almanac.rotate()    ]--> P_rot    (re-orient into target frame axes)
//! 3. Dynamic: P_rot    --[Almanac.translate() ]--> P_target (shift origin to target frame origin)
//! ```
//!
//! Positions use `Point3` (isometry applies translation + rotation).
//! Orientations use `UnitQuaternion` (rotation only — no origin shift).
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
//! The `duration_centuries` + `duration_ns` fields are stored as durations from the J2000 TAI
//! epoch (2000-01-01T12:00:00 TAI). The `timescale_id` field identifies the original measurement
//! timescale but is NOT currently applied to epoch conversion. TAI is always used internally.
//! The difference matters at the ~37s level (TAI − UTC), which is non-negligible for precise
//! orbit determination but acceptable for visualization and simulation use cases.
//!
//! # Unit Convention for FrameRegistry Translations
//!
//! Registry translations are assumed to be in the same physical units as the row's `units_pos`
//! field. A batch where all rows share one unit is the intended use case.

use anise::prelude::*;
use arrow::array::{
    Array, DictionaryArray, FixedSizeListArray, Float64Array, Int16Array, Int16Builder,
    StringArray, StringDictionaryBuilder, StructArray, UInt64Array, UInt64Builder,
};
use arrow::datatypes::{UInt16Type, UInt32Type};
use arrow::record_batch::RecordBatch;
use hifitime::TimeScale;
use nalgebra::{Isometry3, Point3, Quaternion, Rotation3, Translation3, UnitQuaternion, Vector3};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use crate::ephemeris::{epoch_from_parts, epoch_to_parts};
use crate::schema::{
    FrameRegistry, STS_COLUMN, STS_REGISTRY_METADATA_KEY, SpaceTimestampBuilder, is_entity_uri,
};

/// Converts a position/velocity unit string to a multiplier yielding kilometers.
fn unit_to_km_factor(unit: &str) -> f64 {
    match unit.to_lowercase().as_str() {
        "m" | "meters" | "meter" => 0.001,
        "km" | "kilometers" | "kilometer" => 1.0,
        "au" => 149_597_870.7,
        _ => 1.0,
    }
}

/// Reads a flat `[x, y, z]` triple from the raw values buffer of a `FixedSizeListArray`,
/// correctly accounting for the list array's Arrow offset (present in sliced batches).
#[inline]
fn read_vec3(values: &Float64Array, list_offset: usize, row: usize) -> [f64; 3] {
    let base = (list_offset + row) * 3;
    [
        values.value(base),
        values.value(base + 1),
        values.value(base + 2),
    ]
}

/// Reads a flat `[w, x, y, z]` quadruple from the raw values buffer of a `FixedSizeListArray`,
/// correctly accounting for the list array's Arrow offset.
#[inline]
fn read_vec4(values: &Float64Array, list_offset: usize, row: usize) -> [f64; 4] {
    let base = (list_offset + row) * 4;
    [
        values.value(base),
        values.value(base + 1),
        values.value(base + 2),
        values.value(base + 3),
    ]
}

/// A resolved dynamic frame: the astronomical root its chain terminates in, plus the
/// isometry (always in km) taking child-frame coordinates into that root.
pub type ResolvedFrame = (String, Isometry3<f64>);

/// Resolves an entity-URI frame name at a given epoch — see [`transform_batch`].
///
/// `soloc`'s ledger supplies one of these; `spacetimestamp` never looks up poses itself.
pub type DynamicFrameResolver<'a> = &'a dyn Fn(&str, Epoch) -> Option<ResolvedFrame>;

/// Transforms the `spacetimestamp` struct column of a batch into a new target astronomical frame.
///
/// This function acts as a pure projection: the original `RecordBatch` is unmodified.
/// A new `RecordBatch` is returned with only the `"spacetimestamp"` struct column replaced
/// (reprojected position, quaternion, and time fields). All other columns — including
/// `velocity`, `angular_velocity`, `acceleration`, `entity_id`, etc. — are passed through
/// unchanged. The caller is responsible for reprojecting those kinematic fields if needed.
///
/// # Arguments
/// * `batch` - The immutable source data. Must contain a `"spacetimestamp"` struct column.
/// * `target_frame_name` - The target `anise` frame (e.g. "ICRF", "Earth").
/// * `almanac` - The `anise` ephemeris engine holding planetary data.
/// * `target_unit` - The desired output unit for position (e.g. "km" or "m").
/// * `resolve_dynamic_frame` - Optional resolver called when a row's `frame_id` is an
///   entity URI (e.g. `"demo:truck_A"`) whose pose must be looked up in a ledger. It
///   receives the frame name and **that row's own epoch**, and returns
///   `(astronomical_root, isometry_km)` — the isometry translating child-frame
///   coordinates (in km) into the astronomical root frame.
///
///   Taking a resolver rather than a prebuilt map is what lets a batch spanning several
///   timesteps resolve each row against the pose that was current *at that row's epoch*;
///   a single map could only ever hold one epoch's answer for the whole batch. Results
///   are memoised per `(frame, epoch)`, so a snapshot batch where every row shares a
///   frame and timestep still costs exactly one resolver call.
///
///   Dynamic frame isometries are always in km, regardless of the batch's `units_pos`.
///   Static [`FrameRegistry`] entries are checked first; dynamic frames are the fallback.
pub fn transform_batch(
    batch: &RecordBatch,
    target_frame_name: &str,
    almanac: &Almanac,
    target_unit: &str,
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

    let schema = batch.schema();
    let col_idx = schema
        .index_of(STS_COLUMN)
        .map_err(|_| format!("Column '{}' not found", STS_COLUMN))?;

    let sts_col = batch.column(col_idx);
    let struct_array = sts_col
        .as_any()
        .downcast_ref::<StructArray>()
        .ok_or_else(|| format!("'{}' column is not a StructArray", STS_COLUMN))?;

    // 1. Extract the registry from the schema's root metadata
    let registry = schema
        .metadata()
        .get(STS_REGISTRY_METADATA_KEY)
        .and_then(|json| FrameRegistry::from_json(json).ok());

    // 2. Pre-compute static custom frame transforms.
    //
    // For each custom frame in the registry, walk up the parent chain until reaching
    // an external astronomical frame (one not in the registry). Compose all intermediate
    // isometries into a single cached Isometry3. This avoids repeated tree traversals
    // in the hot row-processing loop.
    //
    // The composition order is:  iso = T_grandparent * T_parent * T_child
    // Applied as:                P_root = iso * P_local  (using Point3 to include translation)
    //
    // Maps fully-qualified frame ID → (astronomical_root_name, composed_isometry)
    let mut custom_frame_cache: HashMap<String, (String, Isometry3<f64>)> = HashMap::new();

    if let Some(ref reg) = registry {
        for frame_id in reg.frames.keys() {
            let mut current = frame_id.clone();
            let mut iso = Isometry3::identity();

            loop {
                if let Some(transform) = reg.frames.get(&current) {
                    let trans = Translation3::new(
                        transform.translation[0],
                        transform.translation[1],
                        transform.translation[2],
                    );
                    let quat = UnitQuaternion::from_quaternion(Quaternion::new(
                        transform.rotation_quat[0], // w
                        transform.rotation_quat[1], // x
                        transform.rotation_quat[2], // y
                        transform.rotation_quat[3], // z
                    ));
                    // Prepend the parent node's transform: T_parent * accumulated
                    iso = Isometry3::from_parts(trans, quat) * iso;
                    current = transform.parent_id.clone();
                } else {
                    // Reached a node not in the registry — this is the astronomical root.
                    custom_frame_cache.insert(frame_id.clone(), (current, iso));
                    break;
                }
            }
        }
    }

    // 3. Extract child arrays for fast vectorized access.
    //    Dictionary arrays: look up the key index, then use it to index the values array.
    //    FixedSizeList arrays: the values buffer starts at offset 0 of the UNDERLYING buffer,
    //    but the list array itself has an `offset()` field that marks where valid data begins.
    //    We pass `list.offset()` into the read_vec* helpers so sliced batches are handled correctly.
    let frames = struct_array
        .column_by_name("frame_id")
        .unwrap()
        .as_any()
        .downcast_ref::<DictionaryArray<UInt32Type>>()
        .unwrap();
    let frames_dict = frames
        .values()
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();

    let units = struct_array
        .column_by_name("units_pos")
        .unwrap()
        .as_any()
        .downcast_ref::<DictionaryArray<UInt16Type>>()
        .unwrap();
    let units_dict = units
        .values()
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();

    let timescales = struct_array
        .column_by_name("timescale_id")
        .unwrap()
        .as_any()
        .downcast_ref::<DictionaryArray<UInt32Type>>()
        .unwrap();
    let timescales_dict = timescales
        .values()
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();

    let sources = struct_array
        .column_by_name("source_id")
        .unwrap()
        .as_any()
        .downcast_ref::<DictionaryArray<UInt32Type>>()
        .unwrap();
    let sources_dict = sources
        .values()
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();

    let estimates = struct_array
        .column_by_name("estimate_type")
        .unwrap()
        .as_any()
        .downcast_ref::<DictionaryArray<UInt16Type>>()
        .unwrap();
    let estimates_dict = estimates
        .values()
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();

    let pos_list = struct_array
        .column_by_name("position")
        .unwrap()
        .as_any()
        .downcast_ref::<FixedSizeListArray>()
        .unwrap();
    let pos_values = pos_list
        .values()
        .as_any()
        .downcast_ref::<Float64Array>()
        .unwrap();
    let pos_offset = pos_list.offset();

    let quat_list = struct_array
        .column_by_name("quaternion")
        .unwrap()
        .as_any()
        .downcast_ref::<FixedSizeListArray>()
        .unwrap();
    let quat_values = quat_list
        .values()
        .as_any()
        .downcast_ref::<Float64Array>()
        .unwrap();
    let quat_offset = quat_list.offset();

    let cent_arr = struct_array
        .column_by_name("duration_centuries")
        .unwrap()
        .as_any()
        .downcast_ref::<Int16Array>()
        .unwrap();
    let ns_arr = struct_array
        .column_by_name("duration_ns")
        .unwrap()
        .as_any()
        .downcast_ref::<UInt64Array>()
        .unwrap();

    let num_rows = batch.num_rows();
    let mut sts_builder = SpaceTimestampBuilder::new(num_rows, registry.clone());

    let to_target_factor = 1.0 / unit_to_km_factor(target_unit);

    // Memoises resolver answers per (frame dictionary key, epoch). Keyed on the dictionary
    // key rather than the frame string so a hit costs no allocation; the key uniquely
    // identifies a name within this batch. A per-timestep snapshot batch therefore makes
    // one resolver call, not one per row.
    let mut dynamic_cache: HashMap<(u32, (i16, u64)), Option<ResolvedFrame>> = HashMap::new();

    // 4. Iterate over the data and apply transformations.
    for i in 0..num_rows {
        // Look up strings via dictionary key → value index
        let frame_key = frames.keys().value(i);
        let original_frame = frames_dict.value(frame_key as usize);
        let current_unit = units_dict.value(units.keys().value(i) as usize);
        let ts_str = timescales_dict.value(timescales.keys().value(i) as usize);
        let source_str = sources_dict.value(sources.keys().value(i) as usize);
        let est_str = estimates_dict.value(estimates.keys().value(i) as usize);

        // Read position, quaternion (with Arrow offset accounting)
        let [px, py, pz] = read_vec3(pos_values, pos_offset, i);
        let [qw, qx, qy, qz] = read_vec4(quat_values, quat_offset, i);

        let centuries = cent_arr.value(i);
        let ns = ns_arr.value(i);

        // Reconstruct the physical epoch from the stored (centuries, ns) and their declared
        // timescale. epoch_from_parts applies the correct J2000 reference for that timescale
        // so the resulting Epoch is always physically correct regardless of storage timescale.
        let timescale = TimeScale::from_str(ts_str).unwrap_or(TimeScale::TAI);
        let epoch = epoch_from_parts(centuries, ns, timescale);

        let to_km = unit_to_km_factor(current_unit);

        // --- Stage 1: Local frame → astronomical root ---
        //
        // Three possible resolutions, checked in priority order:
        //   a) Static FrameRegistry entry  — isometry is in batch native units
        //   b) Dynamic frame from caller   — isometry is in km (entity pose from ledger)
        //   c) Passthrough                 — original_frame is already an astronomical root
        let quat_local = UnitQuaternion::from_quaternion(Quaternion::new(qw, qx, qy, qz));

        // Resolved against this row's own epoch, so a batch spanning several timesteps
        // gets the pose that was current at each one.
        let dynamic = resolve_dynamic_frame.and_then(|resolve| {
            dynamic_cache
                .entry((frame_key, epoch.to_tai_duration().to_parts()))
                .or_insert_with(|| resolve(original_frame, epoch))
                .clone()
        });

        let (root_frame_name, pos_root, quat_root) =
            if let Some((root, iso)) = custom_frame_cache.get(original_frame) {
                // Static: isometry in batch units; convert to km after applying.
                let pos_root = (iso * Point3::new(px, py, pz)).coords * to_km;
                let quat_root = iso.rotation * quat_local;
                (root.clone(), pos_root, quat_root)
            } else if let Some((root, iso)) = dynamic {
                // Dynamic: isometry in km; normalize coordinates to km first.
                let pos_km = Point3::new(px * to_km, py * to_km, pz * to_km);
                let pos_root = (iso * pos_km).coords;
                let quat_root = iso.rotation * quat_local;
                (root, pos_root, quat_root)
            } else {
                // Passthrough: original_frame is an astronomical root already.
                let pos_root = Vector3::new(px, py, pz) * to_km;
                (original_frame.to_string(), pos_root, quat_local)
            };

        // --- Stage 2 & 3: Dynamic transform (astronomical root → target) via Almanac ---
        //
        // Root frame names are always bare astronomical identifiers: "ICRF", "IAU_EARTH",
        // "IAU_TITAN", etc. Entity URIs are resolved before reaching this point and never
        // appear here as root_frame_name.
        let root_frame = crate::ephemeris::resolve_astronomical_frame(&root_frame_name)
            .ok_or_else(|| {
                // An entity-shaped name reaching this point means the resolver could not
                // place it — say so, rather than blaming anise for a name it never owned.
                if is_entity_uri(&root_frame_name) {
                    format!(
                        "Failed resolving frame '{root_frame_name}': it looks like an entity \
                         reference, but no pose for it could be resolved at {epoch}"
                    )
                } else {
                    format!(
                        "Failed resolving root frame '{}': not recognized by anise",
                        root_frame_name
                    )
                }
            })?;

        // almanac.translate(from, to, epoch) → radius_km is the position of `from`'s origin
        // expressed in `to` frame coordinates. This is the additive shift that maps a vector
        // already rotated into `to` orientation from the `from` origin to the `to` origin.
        let translation = almanac
            .translate(root_frame, target_frame, epoch, None)
            .map_err(|e| {
                format!("Anise translate error ('{root_frame_name}' → '{target_frame_name}'): {e}")
            })?;
        let pos_root_wrt_target = translation.radius_km;

        // almanac.rotate(from, to) → DCM R such that v_target = R * v_root
        let dcm = almanac
            .rotate(root_frame, target_frame, epoch)
            .map_err(|e| {
                format!("Anise rotate error ('{root_frame_name}' → '{target_frame_name}'): {e}")
            })?;
        let rot_matrix: Rotation3<f64> = Rotation3::from_matrix_unchecked(dcm.rot_mat);
        let rot_quat: UnitQuaternion<f64> = UnitQuaternion::from_rotation_matrix(&rot_matrix);

        // Compose final position and orientation (all quantities in km at this point)
        let pos_target = rot_matrix * pos_root + pos_root_wrt_target;
        let quat_target = rot_quat * quat_root;

        // Convert position back to the requested output unit
        let pos_out = pos_target * to_target_factor;

        // TODO(covariance): transform_batch does not currently propagate position_covariance
        // or orientation_covariance. Transforming covariance requires applying the rotation
        // Jacobian: C' = R·C·Rᵀ. Until implemented, covariance is set to null in the output
        // to avoid silently producing covariance expressed in the wrong frame.
        sts_builder.append_spacetimestamp(
            target_frame_name,
            target_unit,
            ts_str,
            source_str,
            est_str,
            [pos_out.x, pos_out.y, pos_out.z],
            [quat_target.w, quat_target.i, quat_target.j, quat_target.k],
            centuries,
            ns,
            None, // position_covariance — see TODO above
            None, // orientation_covariance — see TODO above
        );
    }

    // 5. Reconstruct the final RecordBatch, replacing only the spacetimestamp struct column.
    // All other columns, including in parent schemas are passed through as-is;
    // the caller is responsible for reprojecting those if needed.
    let mut final_columns = batch.columns().to_vec();
    final_columns[col_idx] = Arc::new(sts_builder.finish_as_struct());

    RecordBatch::try_new(schema, final_columns).map_err(|e| format!("Batch rebuild error: {e}"))
}

/// Rewrites the time fields in the named spacetimestamp struct column so every row is stored
/// relative to J2000 TAI, regardless of the original `timescale_id`.
///
/// After normalization, `timescale_id` is `"TAI"` for all rows and `(duration_centuries,
/// duration_ns)` are SI-second offsets from `2000-01-01T12:00:00 TAI`. Rows already in TAI
/// are passed through unchanged (fast path when all rows are TAI — returns a cheap clone).
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

    let timescales = struct_array
        .column_by_name("timescale_id")
        .unwrap()
        .as_any()
        .downcast_ref::<DictionaryArray<UInt32Type>>()
        .unwrap();
    let ts_dict = timescales
        .values()
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();

    // Fast path: every row is already TAI — nothing to do.
    let all_tai =
        (0..timescales.len()).all(|i| ts_dict.value(timescales.keys().value(i) as usize) == "TAI");
    if all_tai {
        return Ok(batch.clone());
    }

    let cent_arr = struct_array
        .column_by_name("duration_centuries")
        .unwrap()
        .as_any()
        .downcast_ref::<Int16Array>()
        .unwrap();
    let ns_arr = struct_array
        .column_by_name("duration_ns")
        .unwrap()
        .as_any()
        .downcast_ref::<UInt64Array>()
        .unwrap();

    let num_rows = batch.num_rows();
    let mut new_cents = Int16Builder::with_capacity(num_rows);
    let mut new_ns = UInt64Builder::with_capacity(num_rows);
    let mut new_ts: StringDictionaryBuilder<UInt32Type> =
        StringDictionaryBuilder::with_capacity(num_rows, 1, 3);

    for i in 0..num_rows {
        let ts_str = ts_dict.value(timescales.keys().value(i) as usize);
        let (c, n) = if ts_str == "TAI" {
            (cent_arr.value(i), ns_arr.value(i))
        } else {
            let ts = TimeScale::from_str(ts_str).unwrap_or(TimeScale::TAI);
            let epoch = epoch_from_parts(cent_arr.value(i), ns_arr.value(i), ts);
            epoch_to_parts(epoch)
        };
        new_cents.append_value(c);
        new_ns.append_value(n);
        new_ts.append_value("TAI");
    }

    // Rebuild the struct: replace only the three time-related child arrays.
    let struct_fields = struct_array.fields().clone();
    let mut new_children: Vec<Arc<dyn Array>> =
        struct_array.columns().iter().map(Arc::clone).collect();

    let ts_idx = struct_fields
        .iter()
        .position(|f| f.name() == "timescale_id")
        .unwrap();
    let cent_idx = struct_fields
        .iter()
        .position(|f| f.name() == "duration_centuries")
        .unwrap();
    let ns_idx = struct_fields
        .iter()
        .position(|f| f.name() == "duration_ns")
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
    use arrow::array::{Array, FixedSizeListArray};
    use arrow::datatypes::{DataType, Field, Schema};

    use crate::schema::{SpaceTimestampBuilder, sts_schema};

    /// Build a one-column RecordBatch wrapping a SpaceTimestamp StructArray.
    /// The schema metadata carries the FrameRegistry so transform_batch can find it.
    fn make_sts_batch(
        builder: &mut SpaceTimestampBuilder,
        reg: Option<&FrameRegistry>,
    ) -> RecordBatch {
        let struct_array = builder.finish_as_struct();
        let sts_ref = sts_schema(reg);
        let schema = Arc::new(
            Schema::new(vec![Field::new(
                "spacetimestamp",
                DataType::Struct(sts_ref.fields().clone()),
                false,
            )])
            .with_metadata(sts_ref.metadata().clone()),
        );
        RecordBatch::try_new(schema, vec![Arc::new(struct_array)]).unwrap()
    }

    fn read_output_pos(result: &RecordBatch, row: usize) -> [f64; 3] {
        let sts = result
            .column(0)
            .as_any()
            .downcast_ref::<StructArray>()
            .unwrap();
        let pos_list = sts
            .column_by_name("position")
            .unwrap()
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .unwrap();
        let vals = pos_list
            .values()
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap();
        let base = (pos_list.offset() + row) * 3;
        [vals.value(base), vals.value(base + 1), vals.value(base + 2)]
    }

    fn read_output_quat(result: &RecordBatch, row: usize) -> [f64; 4] {
        let sts = result
            .column(0)
            .as_any()
            .downcast_ref::<StructArray>()
            .unwrap();
        let q_list = sts
            .column_by_name("quaternion")
            .unwrap()
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .unwrap();
        let vals = q_list
            .values()
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap();
        let base = (q_list.offset() + row) * 4;
        [
            vals.value(base),
            vals.value(base + 1),
            vals.value(base + 2),
            vals.value(base + 3),
        ]
    }

    /// Translating from one static-offset custom frame to its immediate parent.
    /// cam is at [1, 0, 0] m from Earth. A point at the cam origin should appear
    /// at [1, 0, 0] m in Earth frame.
    #[test]
    fn test_static_single_hop_translation() {
        let mut reg = FrameRegistry::new_with_namespace("bot");
        reg.add_frame("cam", "Earth", [1.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0]);

        let mut builder = SpaceTimestampBuilder::new(1, Some(reg.clone()));
        builder.append_spacetimestamp(
            "cam",
            "m",
            "TAI",
            "s",
            "MEASURED",
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            0,
            None,
            None,
        );

        let batch = make_sts_batch(&mut builder, Some(&reg));
        let result = transform_batch(&batch, "Earth", &Almanac::default(), "m", None).unwrap();

        let pos = read_output_pos(&result, 0);
        assert_eq!(pos, [1.0, 0.0, 0.0]);
    }

    /// Two-hop chain: cam → base_link → Earth.
    /// base_link is at [0, 1, 0] m from Earth.
    /// cam is at [1, 0, 0] m from base_link.
    /// A point at cam origin should appear at [1, 1, 0] m in Earth frame.
    #[test]
    fn test_static_two_hop_chain() {
        let mut reg = FrameRegistry::new_with_namespace("bot");
        reg.add_frame("base_link", "Earth", [0.0, 1.0, 0.0], [1.0, 0.0, 0.0, 0.0]);
        reg.add_frame("cam", "base_link", [1.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0]);

        let mut builder = SpaceTimestampBuilder::new(1, Some(reg.clone()));
        builder.append_spacetimestamp(
            "cam",
            "m",
            "TAI",
            "s",
            "MEASURED",
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            0,
            None,
            None,
        );

        let batch = make_sts_batch(&mut builder, Some(&reg));
        let result = transform_batch(&batch, "Earth", &Almanac::default(), "m", None).unwrap();

        let pos = read_output_pos(&result, 0);
        assert!((pos[0] - 1.0).abs() < 1e-10, "x={}", pos[0]);
        assert!((pos[1] - 1.0).abs() < 1e-10, "y={}", pos[1]);
        assert!(pos[2].abs() < 1e-10, "z={}", pos[2]);
    }

    /// Static rotation: cam is rotated 90° about Z relative to Earth (no translation).
    /// A point along +X in cam frame should appear along +Y in Earth frame.
    /// The output quaternion should reflect the same 90°-Z rotation.
    #[test]
    fn test_static_rotation_only() {
        use std::f64::consts::FRAC_PI_2;
        // 90° rotation about Z: quaternion = [cos(45°), 0, 0, sin(45°)] = [√2/2, 0, 0, √2/2]
        let half = FRAC_PI_2 / 2.0;
        let (cos_h, sin_h) = (half.cos(), half.sin());

        let mut reg = FrameRegistry::new_with_namespace("bot");
        // rotation_quat convention is [w, x, y, z]
        reg.add_frame("cam", "Earth", [0.0, 0.0, 0.0], [cos_h, 0.0, 0.0, sin_h]);

        let mut builder = SpaceTimestampBuilder::new(1, Some(reg.clone()));
        // Point at [1, 0, 0] in cam frame; after 90°-Z rotation it should be at [0, 1, 0] in Earth.
        builder.append_spacetimestamp(
            "cam",
            "m",
            "TAI",
            "s",
            "MEASURED",
            [1.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0], // identity orientation of the sensor itself
            0,
            0,
            None,
            None,
        );

        let batch = make_sts_batch(&mut builder, Some(&reg));
        let result = transform_batch(&batch, "Earth", &Almanac::default(), "m", None).unwrap();

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

    /// Mixed batch: row 0 is already in Earth frame (no custom frame), row 1 is in a custom
    /// frame at [2, 0, 0] m from Earth. Both should transform correctly in one call.
    #[test]
    fn test_mixed_frame_batch() {
        let mut reg = FrameRegistry::new_with_namespace("bot");
        reg.add_frame("arm", "Earth", [2.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0]);

        let mut builder = SpaceTimestampBuilder::new(2, Some(reg.clone()));
        // Row 0: already in Earth frame at position [5, 0, 0]
        builder.append_spacetimestamp(
            "Earth",
            "m",
            "TAI",
            "s",
            "MEASURED",
            [5.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            0,
            None,
            None,
        );
        // Row 1: in arm frame at [0, 0, 0] → should become [2, 0, 0] in Earth
        builder.append_spacetimestamp(
            "arm",
            "m",
            "TAI",
            "s",
            "MEASURED",
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            0,
            None,
            None,
        );

        let batch = make_sts_batch(&mut builder, Some(&reg));
        let result = transform_batch(&batch, "Earth", &Almanac::default(), "m", None).unwrap();

        let pos0 = read_output_pos(&result, 0);
        assert!((pos0[0] - 5.0).abs() < 1e-10, "row0 x={}", pos0[0]);

        let pos1 = read_output_pos(&result, 1);
        assert!((pos1[0] - 2.0).abs() < 1e-10, "row1 x={}", pos1[0]);
    }

    /// Unit conversion: a point at [1000, 0, 0] m in a frame with no offset
    /// should appear at [1.0, 0, 0] km after transforming with target_unit = "km".
    #[test]
    fn test_unit_conversion_m_to_km() {
        let mut builder = SpaceTimestampBuilder::new(1, None);
        builder.append_spacetimestamp(
            "Earth",
            "m",
            "TAI",
            "s",
            "MEASURED",
            [1000.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            0,
            None,
            None,
        );

        let batch = make_sts_batch(&mut builder, None);
        let result = transform_batch(&batch, "Earth", &Almanac::default(), "km", None).unwrap();

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

    fn read_timescale(batch: &RecordBatch, row: usize) -> String {
        let sts = batch
            .column(0)
            .as_any()
            .downcast_ref::<StructArray>()
            .unwrap();
        let ts_col = sts
            .column_by_name("timescale_id")
            .unwrap()
            .as_any()
            .downcast_ref::<DictionaryArray<UInt32Type>>()
            .unwrap();
        let ts_dict = ts_col
            .values()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        ts_dict.value(ts_col.keys().value(row) as usize).to_string()
    }

    fn read_centuries_ns(batch: &RecordBatch, row: usize) -> (i16, u64) {
        let sts = batch
            .column(0)
            .as_any()
            .downcast_ref::<StructArray>()
            .unwrap();
        let c = sts
            .column_by_name("duration_centuries")
            .unwrap()
            .as_any()
            .downcast_ref::<Int16Array>()
            .unwrap()
            .value(row);
        let n = sts
            .column_by_name("duration_ns")
            .unwrap()
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap()
            .value(row);
        (c, n)
    }

    #[test]
    fn test_normalize_tai_is_noop() {
        let mut builder = SpaceTimestampBuilder::new(2, None);
        builder.append_spacetimestamp(
            "ICRF",
            "km",
            "TAI",
            "s",
            "MEASURED",
            [0.0; 3],
            [1.0, 0.0, 0.0, 0.0],
            0,
            1000,
            None,
            None,
        );
        builder.append_spacetimestamp(
            "ICRF",
            "km",
            "TAI",
            "s",
            "MEASURED",
            [0.0; 3],
            [1.0, 0.0, 0.0, 0.0],
            0,
            2000,
            None,
            None,
        );
        let batch = make_sts_batch(&mut builder, None);
        let result = normalize_batch_to_tai(&batch).unwrap();
        // Should be a cheap clone — same pointer
        assert_eq!(result.num_rows(), 2);
        assert_eq!(read_timescale(&result, 0), "TAI");
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

        let mut builder = SpaceTimestampBuilder::new(1, None);
        builder.append_spacetimestamp(
            "ICRF",
            "km",
            "UTC",
            "s",
            "MEASURED",
            [0.0; 3],
            [1.0, 0.0, 0.0, 0.0],
            utc_c,
            utc_n,
            None,
            None,
        );
        let batch = make_sts_batch(&mut builder, None);

        let result = normalize_batch_to_tai(&batch).unwrap();

        // After normalization: timescale_id should be TAI.
        assert_eq!(read_timescale(&result, 0), "TAI");

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
        // Row 0: TAI — should be unchanged.
        // Row 1: UTC — should be converted.
        let j2000_utc = j2000_in_timescale(TimeScale::UTC);
        let utc_epoch = j2000_utc + hifitime::Duration::from_parts(0, 10_000_000_000u64);
        let (utc_c, utc_n) = (utc_epoch - j2000_utc).to_parts();

        let mut builder = SpaceTimestampBuilder::new(2, None);
        builder.append_spacetimestamp(
            "ICRF",
            "km",
            "TAI",
            "s",
            "MEASURED",
            [0.0; 3],
            [1.0, 0.0, 0.0, 0.0],
            0,
            500,
            None,
            None,
        );
        builder.append_spacetimestamp(
            "ICRF",
            "km",
            "UTC",
            "s",
            "MEASURED",
            [0.0; 3],
            [1.0, 0.0, 0.0, 0.0],
            utc_c,
            utc_n,
            None,
            None,
        );
        let batch = make_sts_batch(&mut builder, None);

        let result = normalize_batch_to_tai(&batch).unwrap();

        assert_eq!(read_timescale(&result, 0), "TAI");
        assert_eq!(read_timescale(&result, 1), "TAI");
        assert_eq!(read_centuries_ns(&result, 0), (0, 500));
        let (c1, n1) = read_centuries_ns(&result, 1);
        assert_eq!(epoch_from_parts(c1, n1, TimeScale::TAI), utc_epoch);
    }
}
