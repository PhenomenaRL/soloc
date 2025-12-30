extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};

/// Returns the Arrow schema for the SpaceTimestamp data.
pub fn schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new(
            "frame_id",
            DataType::Dictionary(Box::new(DataType::UInt16), Box::new(DataType::Utf8)),
            false,
        ),
        Field::new(
            "units_pos",
            DataType::Dictionary(Box::new(DataType::UInt16), Box::new(DataType::Utf8)),
            false,
        ),
        Field::new(
            "timescale_id",
            DataType::Dictionary(Box::new(DataType::UInt16), Box::new(DataType::Utf8)),
            false,
        ),
        Field::new("pos_x", DataType::Float64, false),
        Field::new("pos_y", DataType::Float64, false),
        Field::new("pos_z", DataType::Float64, false),
        Field::new("q_w", DataType::Float32, false),
        Field::new("q_x", DataType::Float32, false),
        Field::new("q_y", DataType::Float32, false),
        Field::new("q_z", DataType::Float32, false),
        Field::new("duration_centuries", DataType::Int16, false),
        Field::new("duration_ns", DataType::UInt64, false),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    use arrow::datatypes::DataType;

    use log::info;
    use test_log::test;

    #[test]
    fn test_schema_definition() {
        let s = schema();
        assert_eq!(s.fields().len(), 12);

        let frame_field = s.field_with_name("frame_id").unwrap();
        match frame_field.data_type() {
            DataType::Dictionary(k, v) => {
                assert_eq!(**k, DataType::UInt16);
                assert_eq!(**v, DataType::Utf8);
            }
            _ => panic!("frame_id should be Dictionary(UInt16, Utf8)"),
        }

        info!("frame_field: {:?}", frame_field);

        let pos_x = s.field_with_name("pos_x").unwrap();
        assert_eq!(pos_x.data_type(), &DataType::Float64);

        let qw = s.field_with_name("q_w").unwrap();
        assert_eq!(qw.data_type(), &DataType::Float32);

        let dur = s.field_with_name("duration_centuries").unwrap();
        assert_eq!(dur.data_type(), &DataType::Int16);

        let dur_ns = s.field_with_name("duration_ns").unwrap();
        assert_eq!(dur_ns.data_type(), &DataType::UInt64);
    }
}
