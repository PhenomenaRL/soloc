//! Transformation Engine for projecting spacetimestamps across astronomical frames.
//!
//! This module provides the core physics engine for `soloc`, taking raw
//! `RecordBatch` data containing embedded `spacetimestamp`s and mathematically
//! projecting their coordinates and velocities into any target reference frame.
//!
//! It combines static graph traversals (using the local `FrameRegistry`) with
//! dynamic, time-varying orbital traversals (using `anise::Almanac`).

use anise::prelude::*;
use arrow::array::{
    Array, AsArray, DictionaryArray, FixedSizeListArray, FixedSizeListBuilder, Float64Array,
    Float64Builder, Int16Array, StringArray, StructArray, UInt64Array,
};
use arrow::datatypes::{UInt16Type, UInt32Type};
use arrow::record_batch::RecordBatch;
use hifitime::{Duration, Epoch, TimeScale};
use nalgebra::{Isometry3, Quaternion, Rotation3, Translation3, UnitQuaternion, Vector3};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use crate::schema::{FrameRegistry, STS_REGISTRY_METADATA_KEY, SpaceTimestampBuilder, sts_schema};

/// Converts a position/velocity unit string to a multiplier yielding kilometers.
fn unit_to_km_factor(unit: &str) -> f64 {
    match unit.to_lowercase().as_str() {
        "m" | "meters" | "meter" => 0.001,
        "km" | "kilometers" | "kilometer" => 1.0,
        "au" => 149_597_870.7,
        _ => 1.0, // Default to assuming kilometers if unknown
    }
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

    // 2. Pre-compute static custom frame transforms
    // Maps a custom frame ID to a tuple of (root_astronomical_frame, static_isometry)
    let mut custom_frame_cache: HashMap<String, (String, Isometry3<f64>)> = HashMap::new();

    if let Some(ref reg) = registry {
        for frame_id in reg.frames.keys() {
            let mut current = frame_id.clone();
            let mut iso = Isometry3::identity();

            // Walk up the tree until we hit an external astronomical frame
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

                    let node_iso = Isometry3::from_parts(trans, quat);
                    iso = node_iso * iso; // Compose the transformation matrix
                    current = transform.parent_id.clone();
                } else {
                    // Reached the astronomical root
                    custom_frame_cache.insert(frame_id.clone(), (current, iso));
                    break;
                }
            }
        }
    }

    // 3. Extract child arrays for fast vectorized access
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

    // Check if we also need to transform a top-level velocity array
    let velocity_col_idx = schema.index_of("velocity").ok();
    let velocity_list = velocity_col_idx.map(|idx| {
        batch
            .column(idx)
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .unwrap()
    });
    let velocity_values = velocity_list.map(|list| {
        list.values()
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap()
    });

    let mut velocity_builder = velocity_col_idx
        .map(|_| FixedSizeListBuilder::new(Float64Builder::with_capacity(batch.num_rows() * 3), 3));

    let num_rows = batch.num_rows();
    let mut sts_builder = SpaceTimestampBuilder::new(num_rows, registry.clone());

    // The J2000 reference epoch for duration offsets
    let j2000_epoch = Epoch::from_str("2000-01-01T12:00:00 TAI").unwrap();
    let to_target_factor = 1.0 / unit_to_km_factor(target_unit);

    // 4. Iterate over the data and apply transformations
    for i in 0..num_rows {
        // Look up strings
        let original_frame = frames_dict.value(frames.keys().value(i) as usize);
        let current_unit = units_dict.value(units.keys().value(i) as usize);
        let ts_str = timescales_dict.value(timescales.keys().value(i) as usize);
        let source_str = sources_dict.value(sources.keys().value(i) as usize);
        let est_str = estimates_dict.value(estimates.keys().value(i) as usize);

        // Fetch numerical inputs
        let px = pos_values.value(i * 3);
        let py = pos_values.value(i * 3 + 1);
        let pz = pos_values.value(i * 3 + 2);

        let qw = quat_values.value(i * 4);
        let qx = quat_values.value(i * 4 + 1);
        let qy = quat_values.value(i * 4 + 2);
        let qz = quat_values.value(i * 4 + 3);

        let centuries = cent_arr.value(i);
        let ns = ns_arr.value(i);

        // Extract Optional Velocity
        let vel_opt = if let (Some(v_vals), Some(v_list)) = (velocity_values, velocity_list) {
            if v_list.is_null(i) {
                None
            } else {
                Some([
                    v_vals.value(i * 3),
                    v_vals.value(i * 3 + 1),
                    v_vals.value(i * 3 + 2),
                ])
            }
        } else {
            None
        };

        // Construct Anise Epoch
        // Converting centuries and nanoseconds back to duration from J2000 TAI
        let _timescale = TimeScale::from_str(ts_str).unwrap_or(TimeScale::UTC);
        let duration = Duration::from_parts(centuries, ns);
        let epoch = j2000_epoch + duration; // TODO: Accurately apply timescale offsets if needed

        // Convert inputs to km and km/s for Anise compatibility
        let to_km = unit_to_km_factor(current_unit);
        let vel_local = match vel_opt {
            Some([vx, vy, vz]) => Vector3::new(vx, vy, vz) * to_km,
            None => Vector3::zeros(),
        };
        let quat_local = UnitQuaternion::from_quaternion(Quaternion::new(qw, qx, qy, qz));

        // Resolve Local -> Root Astronomical Transform
        let (root_frame_name, static_iso) = custom_frame_cache
            .get(original_frame)
            .cloned()
            .unwrap_or_else(|| (original_frame.to_string(), Isometry3::identity()));

        // Apply the static isometry to the position as a Point3 (so translation is included)
        // in native units. Registry translations are assumed to be in the same units as the data.
        // Then convert to km for anise.
        let pos_local_native = nalgebra::Point3::new(px, py, pz);
        let pos_root = (static_iso * pos_local_native).coords * to_km;
        let vel_root = static_iso.rotation * vel_local; // Assuming static frame has no relative translation velocity
        let quat_root = static_iso.rotation * quat_local;

        // Fetch Astronomical Ephemeris Data (Root -> Target)
        // If the root_frame_name is fully qualified but not recognized, fallback to the base name
        let root_frame_base = root_frame_name
            .split(':')
            .last()
            .unwrap_or(root_frame_name.as_str());
        let root_frame = Frame::from_name(root_frame_base, "J2000")
            .or_else(|_| Frame::from_name("SSB", root_frame_base))
            .map_err(|e| format!("Failed resolving root frame {}: {}", root_frame_name, e))?;

        // 1. Dynamic Translation (Position & Velocity)
        let translation = almanac
            .translate(root_frame, target_frame, epoch, None)
            .map_err(|e| format!("Anise Translation Error: {}", e))?;

        let pos_root_wrt_target = translation.radius_km;
        let vel_root_wrt_target = translation.velocity_km_s;

        // 2. Dynamic Rotation (Orientation & Angular Velocity Derivative)
        let dcm = almanac
            .rotate(root_frame, target_frame, epoch)
            .map_err(|e| format!("Anise Rotation Error: {}", e))?;

        let rot_matrix = Rotation3::from_matrix_unchecked(dcm.rot_mat);
        let rot_quat = UnitQuaternion::from_rotation_matrix(&rot_matrix);

        // Compose final states
        let pos_target = rot_matrix * pos_root + pos_root_wrt_target;
        let quat_target = rot_quat * quat_root;

        // Full Kinematic Velocity Transform: v_target = R * v_root + v_root_wrt_target + (R_dt * p_root)
        let mut vel_target = rot_matrix * vel_root + vel_root_wrt_target;
        if let Some(r_dt) = dcm.rot_mat_dt {
            vel_target += r_dt * pos_root;
        }

        // Convert back to desired output units
        let pos_out = pos_target * to_target_factor;
        let vel_out = vel_target * to_target_factor;

        // Append to Spacetimestamp builder
        sts_builder.append_spacetimestamp(
            target_frame_name,
            target_unit, // the new unit
            ts_str,
            source_str,
            est_str,
            [pos_out.x, pos_out.y, pos_out.z],
            [quat_target.w, quat_target.i, quat_target.j, quat_target.k],
            centuries,
            ns,
        );

        // Append to Velocity builder (if applicable)
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

    // 5. Reconstruct the final RecordBatch
    let mut final_columns = batch.columns().to_vec();

    // Replace Spacetimestamp column
    final_columns[col_idx] = Arc::new(sts_builder.finish_as_struct());

    // Replace Velocity column if we built it
    if let (Some(idx), Some(mut vb)) = (velocity_col_idx, velocity_builder) {
        final_columns[idx] = Arc::new(vb.finish());
    }

    // Reconstruct utilizing the exact same schema structure
    RecordBatch::try_new(schema, final_columns).map_err(|e| format!("Batch rebuild error: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::datatypes::{DataType, Field, Schema};

    #[test]
    fn test_transform_batch_static() {
        let mut reg = FrameRegistry::new_with_namespace("test_bot");
        reg.add_frame("cam", "Earth", [1.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0]);

        let mut builder = SpaceTimestampBuilder::new(10, Some(reg.clone()));

        // This will append using the local name "cam" which gets qualified to "test_bot:cam"
        builder.append_spacetimestamp(
            "cam",
            "m",
            "UTC",
            "sensor_1",
            "MEASURED",
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            0,
        );

        let struct_array = builder.finish_as_struct();
        let sts_ref = sts_schema(Some(&reg));

        // We must attach the metadata to the ROOT schema for the transformer to find it
        let schema = Arc::new(
            Schema::new(vec![Field::new(
                "spacetimestamp",
                DataType::Struct(sts_ref.fields().clone()),
                false,
            )])
            .with_metadata(sts_ref.metadata().clone()),
        );

        let batch = RecordBatch::try_new(schema, vec![Arc::new(struct_array)]).unwrap();
        let almanac = Almanac::default();

        let result = transform_batch(&batch, "spacetimestamp", "Earth", &almanac, "m").unwrap();

        let new_sts = result
            .column(0)
            .as_any()
            .downcast_ref::<StructArray>()
            .unwrap();
        let pos_col = new_sts
            .column_by_name("position")
            .unwrap()
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .unwrap();
        let pos_values = pos_col
            .values()
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap();

        assert_eq!(pos_values.value(0), 1.0);
        assert_eq!(pos_values.value(1), 0.0);
        assert_eq!(pos_values.value(2), 0.0);
    }
}
