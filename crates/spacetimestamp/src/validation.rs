use arrow::record_batch::RecordBatch;
use arrow::array::{Array, AsArray, DictionaryArray};
use arrow::datatypes::UInt32Type;
use hifitime::TimeScale;
use std::str::FromStr;
use crate::schema::{FrameRegistry, STS_REGISTRY_METADATA_KEY};
use anise::prelude::Frame;

/// Validates that the dictionary strings for timescale_id and frame_id
/// comply with hifitime and anise standards, and the local FrameRegistry.
pub fn validate_spacetimestamp_batch(batch: &RecordBatch) -> Result<(), String> {
    // 1. Extract the optional FrameRegistry from the schema metadata
    let schema = batch.schema();
    let registry = schema
        .metadata()
        .get(STS_REGISTRY_METADATA_KEY)
        .and_then(|json| FrameRegistry::from_json(json).ok());

    // 2. Validate Timescales
    if let Some(timescale_col) = batch.column_by_name("timescale_id") {
        let dict_array = timescale_col.as_any().downcast_ref::<DictionaryArray<UInt32Type>>()
            .ok_or_else(|| "timescale_id is not a UInt32 Dictionary".to_string())?;
        
        let values = dict_array.values().as_string::<i32>();
        
        for i in 0..values.len() {
            if values.is_null(i) { continue; }
            let ts_str = values.value(i);
            
            // hifitime validation
            if TimeScale::from_str(ts_str).is_err() {
                return Err(format!("Invalid timescale: '{}' is not recognized by hifitime", ts_str));
            }
        }
    }

    // 3. Validate Frames
    if let Some(frame_col) = batch.column_by_name("frame_id") {
        let dict_array = frame_col.as_any().downcast_ref::<DictionaryArray<UInt32Type>>()
            .ok_or_else(|| "frame_id is not a UInt32 Dictionary".to_string())?;
            
        let values = dict_array.values().as_string::<i32>();
        
        for i in 0..values.len() {
            if values.is_null(i) { continue; }
            let frame_str = values.value(i);
            
            // a) Is it in the local FrameRegistry?
            if let Some(reg) = &registry
                && reg.frames.contains_key(frame_str) {
                    continue;
                }
            
            // b) Is it a raw NAIF ID?
            if frame_str.parse::<i32>().is_ok() {
                continue;
            }
            
            // c) ICRF/J2000 standard fallbacks
            if frame_str == "ICRF" || frame_str == "J2000" || frame_str == "EME2000" || frame_str == "IAU_MARS" || frame_str == "IAU_EARTH" {
                continue;
            }
            
            // d) Validate via Anise if it's a compound name like "Earth_J2000"
            let is_compound = frame_str.split_once('_').map(|(center, orient)| {
                Frame::from_name(center, orient).is_ok()
            }).unwrap_or(false);
            
            if is_compound {
                continue;
            }

            return Err(format!("Invalid frame_id: '{}' is not recognized by anise or the FrameRegistry", frame_str));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::SpaceTimestampBuilder;

    #[test]
    fn test_valid_batch() {
        let mut builder = SpaceTimestampBuilder::new(10, None);
        builder.append_spacetimestamp(
            "ICRF",
            "m",
            "TAI",
            "sensor_1",
            "MEASURED",
            [0.0; 3],
            [1.0, 0.0, 0.0, 0.0],
            0,
            0,
        );
        let batch = builder.flush();
        assert!(validate_spacetimestamp_batch(&batch).is_ok());
    }

    #[test]
    fn test_invalid_timescale() {
        let mut builder = SpaceTimestampBuilder::new(10, None);
        builder.append_spacetimestamp(
            "ICRF",
            "m",
            "NOT_A_TIMESCALE",
            "sensor_1",
            "MEASURED",
            [0.0; 3],
            [1.0, 0.0, 0.0, 0.0],
            0,
            0,
        );
        let batch = builder.flush();
        let result = validate_spacetimestamp_batch(&batch);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid timescale: 'NOT_A_TIMESCALE'"));
    }

    #[test]
    fn test_invalid_frame() {
        let mut builder = SpaceTimestampBuilder::new(10, None);
        builder.append_spacetimestamp(
            "INVALID_FRAME",
            "m",
            "TAI",
            "sensor_1",
            "MEASURED",
            [0.0; 3],
            [1.0, 0.0, 0.0, 0.0],
            0,
            0,
        );
        let batch = builder.flush();
        let result = validate_spacetimestamp_batch(&batch);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid frame_id: 'INVALID_FRAME'"));
    }
    
    #[test]
    fn test_valid_custom_frame() {
        let mut reg = FrameRegistry::new_with_namespace("robot");
        reg.add_frame("cam", "ICRF", [0.0; 3], [1.0, 0.0, 0.0, 0.0]);
        
        let mut builder = SpaceTimestampBuilder::new(10, Some(reg));
        builder.append_spacetimestamp(
            "cam", // will become "robot:cam"
            "m",
            "UTC",
            "sensor_1",
            "MEASURED",
            [0.0; 3],
            [1.0, 0.0, 0.0, 0.0],
            0,
            0,
        );
        let batch = builder.flush();
        assert!(validate_spacetimestamp_batch(&batch).is_ok());
    }
}
