extern crate alloc;

use alloc::boxed::Box;
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

    #[test]
    fn test_recordbatch_generation_and_sampling() {
        use arrow::array::{
            Array, Float32Array, Float64Array, Int16Array, StringDictionaryBuilder, UInt64Array,
        };
        use arrow::datatypes::UInt16Type;
        use arrow::record_batch::RecordBatch;

        let s = schema();

        // Builders
        let mut frame_id = StringDictionaryBuilder::<UInt16Type>::new();
        let mut units_pos = StringDictionaryBuilder::<UInt16Type>::new();
        let mut timescale_id = StringDictionaryBuilder::<UInt16Type>::new();

        let mut pos_x = Float64Array::builder(10);
        let mut pos_y = Float64Array::builder(10);
        let mut pos_z = Float64Array::builder(10);

        let mut q_w = Float32Array::builder(10);
        let mut q_x = Float32Array::builder(10);
        let mut q_y = Float32Array::builder(10);
        let mut q_z = Float32Array::builder(10);

        let mut dur_c = Int16Array::builder(10);
        let mut dur_ns = UInt64Array::builder(10);

        // Generate 10 rows
        for i in 0..10 {
            frame_id.append_value("EME2000");
            units_pos.append_value("km");
            timescale_id.append_value("TDB");

            pos_x.append_value(i as f64);
            pos_y.append_value(i as f64 * 10.0);
            pos_z.append_value(i as f64 * 100.0);

            q_w.append_value(1.0);
            q_x.append_value(0.0);
            q_y.append_value(0.0);
            q_z.append_value(0.0);

            dur_c.append_value(0);
            dur_ns.append_value(i as u64);
        }

        let batch = RecordBatch::try_new(
            s,
            vec![
                Arc::new(frame_id.finish()),
                Arc::new(units_pos.finish()),
                Arc::new(timescale_id.finish()),
                Arc::new(pos_x.finish()),
                Arc::new(pos_y.finish()),
                Arc::new(pos_z.finish()),
                Arc::new(q_w.finish()),
                Arc::new(q_x.finish()),
                Arc::new(q_y.finish()),
                Arc::new(q_z.finish()),
                Arc::new(dur_c.finish()),
                Arc::new(dur_ns.finish()),
            ],
        )
        .expect("should create record batch");

        info!("Created batch with {} rows", batch.num_rows());
        assert_eq!(batch.num_rows(), 10);

        // Sample a subset
        let subset = batch.slice(2, 4); // Offset 2, length 4
        assert_eq!(subset.num_rows(), 4);

        // Verify data in subset
        let pos_x_col = subset
            .column_by_name("pos_x")
            .unwrap()
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap();

        for i in 0..4 {
            // The value at index `i` in the subset corresponds to `i + 2` in the original data
            assert_eq!(pos_x_col.value(i), (i + 2) as f64);
        }
        info!("Subset verification successful");
    }
}
