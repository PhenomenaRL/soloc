use crate::ephemeris::is_valid_astronomical_frame;
use crate::schema::STS_COLUMN;
use arrow::array::{Array, AsArray, DictionaryArray, StructArray};
use arrow::datatypes::UInt32Type;
use arrow::record_batch::RecordBatch;
use hifitime::TimeScale;
use std::str::FromStr;

/// Validates `timescale_id` and `frame_id` values in a batch against hifitime and anise standards.
///
/// The batch may be either:
/// - A nested entity/ledger batch — containing a `"spacetimestamp"` [`StructArray`] column.
///   The STS fields are read from inside that struct.
/// - A flat STS batch — produced by [`crate::schema::SpaceTimestampBuilder::flush`].
///   The STS fields are top-level columns.
///
/// Detection is automatic: if a `"spacetimestamp"` struct column is present, the nested
/// path is taken; otherwise the top-level columns are checked.
///
/// A `frame_id` is accepted if it is a raw NAIF integer ID, an entity URI (resolved against
/// the ledger's derived topology, not here), or a name anise recognizes. Frame *reachability*
/// is checked at ledger append time by [`crate::topology::TransformTree`]; this function only
/// checks that the identifier is well-formed.
pub fn validate_spacetimestamp_batch(batch: &RecordBatch) -> Result<(), String> {
    // Locate the STS fields — either nested inside STS_COLUMN or at the top level.
    let (timescale_col, frame_col) = match batch.column_by_name(STS_COLUMN) {
        Some(col) => {
            let s = col
                .as_any()
                .downcast_ref::<StructArray>()
                .ok_or_else(|| format!("'{}' column is not a StructArray", STS_COLUMN))?;
            (
                s.column_by_name("timescale_id"),
                s.column_by_name("frame_id"),
            )
        }
        None => (
            batch.column_by_name("timescale_id"),
            batch.column_by_name("frame_id"),
        ),
    };

    // Validate timescales.
    if let Some(col) = timescale_col {
        let dict = col
            .as_any()
            .downcast_ref::<DictionaryArray<UInt32Type>>()
            .ok_or_else(|| "timescale_id is not a UInt32 Dictionary".to_string())?;
        let values = dict.values().as_string::<i32>();
        for i in 0..values.len() {
            if values.is_null(i) {
                continue;
            }
            let ts_str = values.value(i);
            if TimeScale::from_str(ts_str).is_err() {
                return Err(format!(
                    "Invalid timescale: '{}' is not recognized by hifitime",
                    ts_str
                ));
            }
        }
    }

    // Validate frame IDs.
    if let Some(col) = frame_col {
        let dict = col
            .as_any()
            .downcast_ref::<DictionaryArray<UInt32Type>>()
            .ok_or_else(|| "frame_id is not a UInt32 Dictionary".to_string())?;
        let values = dict.values().as_string::<i32>();
        for i in 0..values.len() {
            if values.is_null(i) {
                continue;
            }
            let frame_str = values.value(i);

            if frame_str.parse::<i32>().is_ok() {
                continue;
            }
            if crate::schema::is_entity_uri(frame_str) {
                continue;
            }
            if is_valid_astronomical_frame(frame_str) {
                continue;
            }

            return Err(format!(
                "Invalid frame_id: '{}' is not a NAIF ID, an entity URI, or a frame name \
                 recognized by anise",
                frame_str
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{SpaceTimestampBuilder, sts_schema};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use std::sync::Arc;

    fn make_flat_batch(frame: &str, timescale: &str) -> RecordBatch {
        let mut builder = SpaceTimestampBuilder::new(1);
        builder.append_spacetimestamp(
            frame,
            "km",
            timescale,
            "sensor_1",
            "MEASURED",
            [0.0; 3],
            [1.0, 0.0, 0.0, 0.0],
            0,
            0,
            None,
            None,
        );
        builder.flush()
    }

    fn make_nested_batch(frame: &str, timescale: &str) -> RecordBatch {
        let mut builder = SpaceTimestampBuilder::new(1);
        builder.append_spacetimestamp(
            frame,
            "km",
            timescale,
            "sensor_1",
            "MEASURED",
            [0.0; 3],
            [1.0, 0.0, 0.0, 0.0],
            0,
            0,
            None,
            None,
        );
        let struct_array = builder.finish_as_struct();
        let schema = Arc::new(Schema::new(vec![Field::new(
            STS_COLUMN,
            DataType::Struct(sts_schema().fields().clone()),
            false,
        )]));
        RecordBatch::try_new(schema, vec![Arc::new(struct_array)]).unwrap()
    }

    #[test]
    fn test_valid_flat_batch() {
        assert!(validate_spacetimestamp_batch(&make_flat_batch("ICRF", "TAI")).is_ok());
    }

    #[test]
    fn test_valid_nested_batch() {
        assert!(validate_spacetimestamp_batch(&make_nested_batch("ICRF", "TAI")).is_ok());
    }

    #[test]
    fn test_invalid_timescale_flat() {
        let result = validate_spacetimestamp_batch(&make_flat_batch("ICRF", "NOT_A_TIMESCALE"));
        assert!(result.unwrap_err().contains("Invalid timescale"));
    }

    #[test]
    fn test_invalid_timescale_nested() {
        let result = validate_spacetimestamp_batch(&make_nested_batch("ICRF", "NOT_A_TIMESCALE"));
        assert!(result.unwrap_err().contains("Invalid timescale"));
    }

    #[test]
    fn test_invalid_frame_flat() {
        let result = validate_spacetimestamp_batch(&make_flat_batch("INVALID_FRAME", "TAI"));
        assert!(result.unwrap_err().contains("Invalid frame_id"));
    }

    #[test]
    fn test_invalid_frame_nested() {
        let result = validate_spacetimestamp_batch(&make_nested_batch("INVALID_FRAME", "TAI"));
        assert!(result.unwrap_err().contains("Invalid frame_id"));
    }

    #[test]
    fn test_entity_uri_frame_is_valid() {
        assert!(validate_spacetimestamp_batch(&make_flat_batch("demo:truck_A", "TAI")).is_ok());
        assert!(validate_spacetimestamp_batch(&make_nested_batch("demo:truck_A", "TAI")).is_ok());
    }

    /// A raw NAIF integer ID is a valid frame identifier and must not be rejected.
    #[test]
    fn test_naif_id_frame_is_valid() {
        assert!(validate_spacetimestamp_batch(&make_flat_batch("499", "TAI")).is_ok());
        assert!(validate_spacetimestamp_batch(&make_nested_batch("499", "TAI")).is_ok());
    }

    /// An unqualified local name is no longer legal: without the registry there is no
    /// namespacing step that could turn "cam" into a resolvable identifier.
    #[test]
    fn test_unqualified_local_frame_is_rejected() {
        let result = validate_spacetimestamp_batch(&make_flat_batch("cam", "TAI"));
        assert!(result.unwrap_err().contains("Invalid frame_id"));
    }
}
