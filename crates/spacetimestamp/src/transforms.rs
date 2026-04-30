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
//! Velocities and orientations use `Vector3`/`UnitQuaternion` (rotation only — no origin shift).
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
    Array, DictionaryArray, FixedSizeListArray, FixedSizeListBuilder, Float64Array, Float64Builder,
    Int16Array, StringArray, StructArray, UInt64Array,
};
use arrow::datatypes::{UInt16Type, UInt32Type};
use arrow::record_batch::RecordBatch;
use hifitime::{Duration, Epoch, TimeScale};
use nalgebra::{Isometry3, Quaternion, Rotation3, Translation3, UnitQuaternion, Vector3};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use crate::schema::{FrameRegistry, STS_REGISTRY_METADATA_KEY, SpaceTimestampBuilder};

/// Converts a position/velocity unit string to a multiplier yielding kilometers.
fn unit_to_km_factor(unit: &str) -> f64 {
    match unit.to_lowercase().as_str() {
        "m" | "meters" | "meter" => 0.001,
        "km" | "kilometers" | "kilometer" => 1.0,
        "au" => 149_597_870.7,
        _ => 1.0, // Default to assuming kilometers if unknown
    }
}

/// Reads a flat `[x, y, z]` triple from the raw values buffer of a `FixedSizeListArray`,
/// correctly accounting for the list array's Arrow offset (present in sliced batches).
#[inline]
fn read_vec3(values: &Float64Array, list_offset: usize, row: usize) -> [f64; 3] {
    let base = (list_offset + row) * 3;
    [values.value(base), values.value(base + 1), values.value(base + 2)]
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

/// Transforms a batch containing a `spacetimestamp` (and optionally `velocity`)
/// into a new target astronomical frame.
///
/// This function acts as a pure projection: the original `RecordBatch` is unmodified.
/// A new `RecordBatch` is returned containing the transformed positions, quaternions,
/// and velocities, with all other custom domain columns preserved exactly as they were.
///
/// # Arguments
/// * `batch` - The immutable source data.
/// * `sts_column_name` - The name of the struct column containing the spacetimestamp (e.g. "spacetimestamp").
/// * `target_frame_name` - The target `anise` frame (e.g. "ICRF", "Earth").
/// * `almanac` - The `anise` ephemeris engine holding planetary data.
/// * `target_unit` - The desired output unit for position and velocity (e.g. "km" or "m").
pub fn transform_batch(
    batch: &RecordBatch,
    sts_column_name: &str,
    target_frame_name: &str,
    almanac: &Almanac,
    target_unit: &str,
) -> Result<RecordBatch, String> {
    // Attempt to resolve the target frame in anise. We default to assuming J2000 orientation
    // if the user simply passed a planetary center like "Mars".
    let target_frame = Frame::from_name(target_frame_name, "J2000")
        .or_else(|_| Frame::from_name("SSB", target_frame_name))
        .map_err(|e| format!("Invalid target_frame_name '{}': {}", target_frame_name, e))?;

    let schema = batch.schema();
    let col_idx = schema
        .index_of(sts_column_name)
        .map_err(|_| format!("Column '{}' not found", sts_column_name))?;

    let sts_col = batch.column(col_idx);
    let struct_array = sts_col
        .as_any()
        .downcast_ref::<StructArray>()
        .ok_or_else(|| format!("'{}' column is not a StructArray", sts_column_name))?;

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
    let frames_dict = frames.values().as_any().downcast_ref::<StringArray>().unwrap();

    let units = struct_array
        .column_by_name("units_pos")
        .unwrap()
        .as_any()
        .downcast_ref::<DictionaryArray<UInt16Type>>()
        .unwrap();
    let units_dict = units.values().as_any().downcast_ref::<StringArray>().unwrap();

    let timescales = struct_array
        .column_by_name("timescale_id")
        .unwrap()
        .as_any()
        .downcast_ref::<DictionaryArray<UInt32Type>>()
        .unwrap();
    let timescales_dict = timescales.values().as_any().downcast_ref::<StringArray>().unwrap();

    let sources = struct_array
        .column_by_name("source_id")
        .unwrap()
        .as_any()
        .downcast_ref::<DictionaryArray<UInt32Type>>()
        .unwrap();
    let sources_dict = sources.values().as_any().downcast_ref::<StringArray>().unwrap();

    let estimates = struct_array
        .column_by_name("estimate_type")
        .unwrap()
        .as_any()
        .downcast_ref::<DictionaryArray<UInt16Type>>()
        .unwrap();
    let estimates_dict = estimates.values().as_any().downcast_ref::<StringArray>().unwrap();

    let pos_list = struct_array
        .column_by_name("position")
        .unwrap()
        .as_any()
        .downcast_ref::<FixedSizeListArray>()
        .unwrap();
    let pos_values = pos_list.values().as_any().downcast_ref::<Float64Array>().unwrap();
    let pos_offset = pos_list.offset();

    let quat_list = struct_array
        .column_by_name("quaternion")
        .unwrap()
        .as_any()
        .downcast_ref::<FixedSizeListArray>()
        .unwrap();
    let quat_values = quat_list.values().as_any().downcast_ref::<Float64Array>().unwrap();
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

    // Check if we also need to transform a top-level velocity array.
    let velocity_col_idx = schema.index_of("velocity").ok();
    let velocity_list = velocity_col_idx.map(|idx| {
        batch
            .column(idx)
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .unwrap()
    });
    let velocity_values = velocity_list.map(|list| {
        list.values().as_any().downcast_ref::<Float64Array>().unwrap()
    });
    let vel_offset = velocity_list.map(|l| l.offset()).unwrap_or(0);

    let mut velocity_builder = velocity_col_idx
        .map(|_| FixedSizeListBuilder::new(Float64Builder::with_capacity(batch.num_rows() * 3), 3));

    let num_rows = batch.num_rows();
    let mut sts_builder = SpaceTimestampBuilder::new(num_rows, registry.clone());

    // The J2000 reference epoch for duration offsets (TAI scale).
    // All stored durations are relative to this epoch using TAI, regardless of timescale_id.
    let j2000_epoch = Epoch::from_str("2000-01-01T12:00:00 TAI").unwrap();
    let to_target_factor = 1.0 / unit_to_km_factor(target_unit);

    // 4. Iterate over the data and apply transformations.
    for i in 0..num_rows {
        // Look up strings via dictionary key → value index
        let original_frame = frames_dict.value(frames.keys().value(i) as usize);
        let current_unit = units_dict.value(units.keys().value(i) as usize);
        let ts_str = timescales_dict.value(timescales.keys().value(i) as usize);
        let source_str = sources_dict.value(sources.keys().value(i) as usize);
        let est_str = estimates_dict.value(estimates.keys().value(i) as usize);

        // Read position, quaternion (with Arrow offset accounting)
        let [px, py, pz] = read_vec3(pos_values, pos_offset, i);
        let [qw, qx, qy, qz] = read_vec4(quat_values, quat_offset, i);

        let centuries = cent_arr.value(i);
        let ns = ns_arr.value(i);

        // Extract optional velocity from the top-level column
        let vel_opt = if let (Some(v_vals), Some(v_list)) = (velocity_values, velocity_list) {
            if v_list.is_null(i) {
                None
            } else {
                let [vx, vy, vz] = read_vec3(v_vals, vel_offset, i);
                Some([vx, vy, vz])
            }
        } else {
            None
        };

        // Construct epoch from TAI duration offset from J2000.
        // timescale_id identifies the original measurement context but is not applied here.
        let _timescale = TimeScale::from_str(ts_str).unwrap_or(TimeScale::TAI);
        let duration = Duration::from_parts(centuries, ns);
        let epoch = j2000_epoch + duration;

        let to_km = unit_to_km_factor(current_unit);

        // --- Stage 1: Static transform (local frame → astronomical root) ---
        //
        // Retrieve the pre-composed isometry for this custom frame.
        // Falls back to identity if original_frame is already an astronomical frame.
        let (root_frame_name, static_iso) = custom_frame_cache
            .get(original_frame)
            .cloned()
            .unwrap_or_else(|| (original_frame.to_string(), Isometry3::identity()));

        // Positions are Points: isometry applies BOTH rotation and translation.
        // Apply in native units (registry translations share the data's unit system),
        // then convert to km for the anise boundary.
        let pos_local = nalgebra::Point3::new(px, py, pz);
        let pos_root = (static_iso * pos_local).coords * to_km;

        // Velocities and orientations are vectors/rotors: only the rotational part applies.
        let vel_local: Vector3<f64> = match vel_opt {
            Some([vx, vy, vz]) => Vector3::new(vx, vy, vz) * to_km,
            None => Vector3::zeros(),
        };
        let quat_local: UnitQuaternion<f64> =
            UnitQuaternion::from_quaternion(Quaternion::new(qw, qx, qy, qz));

        let vel_root: Vector3<f64> = static_iso.rotation * vel_local;
        let quat_root: UnitQuaternion<f64> = static_iso.rotation * quat_local;

        // --- Stage 2 & 3: Dynamic transform (astronomical root → target) via Almanac ---
        //
        // Strip any namespace qualifier (e.g., "ns:Earth" → "Earth") so anise can resolve it.
        // Common astronomical roots like "Earth", "Mars", "ICRF" are recognised by
        // Frame::from_name(center, orientation).
        let root_frame_base = root_frame_name.split(':').last().unwrap_or(root_frame_name.as_str());
        let root_frame = Frame::from_name(root_frame_base, "J2000")
            .or_else(|_| Frame::from_name("SSB", root_frame_base))
            .map_err(|e| format!("Failed resolving root frame '{}': {}", root_frame_name, e))?;

        // almanac.translate(from, to, epoch) → radius_km is the position of `from`'s origin
        // expressed in `to` frame coordinates. This is the additive shift that maps a vector
        // already rotated into `to` orientation from the `from` origin to the `to` origin.
        let translation = almanac
            .translate(root_frame, target_frame, epoch, None)
            .map_err(|e| format!("Anise translate error ('{root_frame_base}' → '{target_frame_name}'): {e}"))?;
        let pos_root_wrt_target = translation.radius_km;
        let vel_root_wrt_target = translation.velocity_km_s;

        // almanac.rotate(from, to) → DCM R such that v_target = R * v_root
        let dcm = almanac
            .rotate(root_frame, target_frame, epoch)
            .map_err(|e| format!("Anise rotate error ('{root_frame_base}' → '{target_frame_name}'): {e}"))?;
        let rot_matrix: Rotation3<f64> = Rotation3::from_matrix_unchecked(dcm.rot_mat);
        let rot_quat: UnitQuaternion<f64> = UnitQuaternion::from_rotation_matrix(&rot_matrix);

        // Compose final states (all quantities in km / km·s⁻¹ at this point)
        let pos_target = rot_matrix * pos_root + pos_root_wrt_target;
        let quat_target = rot_quat * quat_root;

        // Full kinematic velocity:  v_B = R·v_A  +  v_{A→B}  +  dR/dt · p_A
        // The dR/dt term accounts for the frame's angular velocity; it is non-zero when
        // rotating between a body-fixed frame (e.g. IAU_Earth) and an inertial frame.
        let mut vel_target = rot_matrix * vel_root + vel_root_wrt_target;
        if let Some(r_dt) = dcm.rot_mat_dt {
            vel_target += r_dt * pos_root;
        }

        // Convert back to the requested output unit
        let pos_out = pos_target * to_target_factor;
        let vel_out = vel_target * to_target_factor;

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

        if let Some(ref mut vb) = velocity_builder {
            if vel_opt.is_some() {
                vb.values().append_value(vel_out.x);
                vb.values().append_value(vel_out.y);
                vb.values().append_value(vel_out.z);
                vb.append(true);
            } else {
                for _ in 0..3 {
                    vb.values().append_null();
                }
                vb.append(false);
            }
        }
    }

    // 5. Reconstruct the final RecordBatch, replacing only the transformed columns.
    let mut final_columns = batch.columns().to_vec();
    final_columns[col_idx] = Arc::new(sts_builder.finish_as_struct());
    if let (Some(idx), Some(mut vb)) = (velocity_col_idx, velocity_builder) {
        final_columns[idx] = Arc::new(vb.finish());
    }

    RecordBatch::try_new(schema, final_columns).map_err(|e| format!("Batch rebuild error: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Array, FixedSizeListArray};
    use arrow::datatypes::{DataType, Field, Schema};

    use crate::schema::{SpaceTimestampBuilder, sts_schema};

    /// Build a one-column RecordBatch wrapping a SpaceTimestamp StructArray.
    /// The schema metadata carries the FrameRegistry so transform_batch can find it.
    fn make_sts_batch(builder: &mut SpaceTimestampBuilder, reg: Option<&FrameRegistry>) -> RecordBatch {
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
        let sts = result.column(0).as_any().downcast_ref::<StructArray>().unwrap();
        let pos_list = sts
            .column_by_name("position")
            .unwrap()
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .unwrap();
        let vals = pos_list.values().as_any().downcast_ref::<Float64Array>().unwrap();
        let base = (pos_list.offset() + row) * 3;
        [vals.value(base), vals.value(base + 1), vals.value(base + 2)]
    }

    fn read_output_quat(result: &RecordBatch, row: usize) -> [f64; 4] {
        let sts = result.column(0).as_any().downcast_ref::<StructArray>().unwrap();
        let q_list = sts
            .column_by_name("quaternion")
            .unwrap()
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .unwrap();
        let vals = q_list.values().as_any().downcast_ref::<Float64Array>().unwrap();
        let base = (q_list.offset() + row) * 4;
        [vals.value(base), vals.value(base + 1), vals.value(base + 2), vals.value(base + 3)]
    }

    /// Translating from one static-offset custom frame to its immediate parent.
    /// cam is at [1, 0, 0] m from Earth. A point at the cam origin should appear
    /// at [1, 0, 0] m in Earth frame.
    #[test]
    fn test_static_single_hop_translation() {
        let mut reg = FrameRegistry::new_with_namespace("bot");
        reg.add_frame("cam", "Earth", [1.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0]);

        let mut builder = SpaceTimestampBuilder::new(1, Some(reg.clone()));
        builder.append_spacetimestamp("cam", "m", "TAI", "s", "MEASURED", [0.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], 0, 0, None, None);

        let batch = make_sts_batch(&mut builder, Some(&reg));
        let result = transform_batch(&batch, "spacetimestamp", "Earth", &Almanac::default(), "m").unwrap();

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
        builder.append_spacetimestamp("cam", "m", "TAI", "s", "MEASURED", [0.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], 0, 0, None, None);

        let batch = make_sts_batch(&mut builder, Some(&reg));
        let result = transform_batch(&batch, "spacetimestamp", "Earth", &Almanac::default(), "m").unwrap();

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
            "cam", "m", "TAI", "s", "MEASURED",
            [1.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0], // identity orientation of the sensor itself
            0, 0, None, None,
        );

        let batch = make_sts_batch(&mut builder, Some(&reg));
        let result = transform_batch(&batch, "spacetimestamp", "Earth", &Almanac::default(), "m").unwrap();

        let pos = read_output_pos(&result, 0);
        assert!(pos[0].abs() < 1e-10, "x should be ~0, got {}", pos[0]);
        assert!((pos[1] - 1.0).abs() < 1e-10, "y should be ~1, got {}", pos[1]);
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
        builder.append_spacetimestamp("Earth", "m", "TAI", "s", "MEASURED", [5.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], 0, 0, None, None);
        // Row 1: in arm frame at [0, 0, 0] → should become [2, 0, 0] in Earth
        builder.append_spacetimestamp("arm", "m", "TAI", "s", "MEASURED", [0.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], 0, 0, None, None);

        let batch = make_sts_batch(&mut builder, Some(&reg));
        let result = transform_batch(&batch, "spacetimestamp", "Earth", &Almanac::default(), "m").unwrap();

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
        builder.append_spacetimestamp("Earth", "m", "TAI", "s", "MEASURED", [1000.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], 0, 0, None, None);

        let batch = make_sts_batch(&mut builder, None);
        let result = transform_batch(&batch, "spacetimestamp", "Earth", &Almanac::default(), "km").unwrap();

        let pos = read_output_pos(&result, 0);
        assert!((pos[0] - 1.0).abs() < 1e-10, "expected 1.0 km, got {}", pos[0]);
    }
}
