//! Space-time coordinate data structures and Arrow schema definitions.
//!
//! This module provides the [`sts_schema`] for representing satellite or celestial
//! positions and orientations, along with the [`SpaceTimestampBuilder`] for
//! efficient, row-oriented ingestion of this data into Arrow [`RecordBatch`]es.

extern crate alloc;

use alloc::boxed::Box;
use alloc::sync::Arc;
use arrow::array::{
    Array, ArrayBuilder, FixedSizeListBuilder, Float64Builder, Int16Builder,
    StringDictionaryBuilder, UInt64Builder,
};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef, UInt16Type};
use arrow::record_batch::RecordBatch;

/// Returns the canonical Arrow schema for SpaceTimestamp data.
///
/// The schema consists of:
/// * `frame_id`: Dictionary-encoded reference frame (e.g., "EME2000").
/// * `units_pos`: Dictionary-encoded units for position (e.g., "km").
/// * `timescale_id`: Dictionary-encoded timescale (e.g., "TDB").
/// * `estimate_type`: Dictionary-encoded source of data (e.g., "MEASURED").
/// * `position`: `FixedSizeList(3, Float64)` containing `[x, y, z]`.
/// * `quaternion`: `FixedSizeList(4, Float64)` containing `[w, x, y, z]`.
/// * `duration_centuries`: Signed 16-bit integer for large time offsets.
/// * `duration_ns`: Unsigned 64-bit integer for nanosecond precision within the century.
pub fn sts_schema() -> SchemaRef {
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
        Field::new(
            "estimate_type",
            DataType::Dictionary(Box::new(DataType::UInt16), Box::new(DataType::Utf8)),
            false,
        ),
        Field::new(
            "position",
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float64, true)), 3),
            false,
        ),
        Field::new(
            "quaternion",
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float64, true)), 4),
            false,
        ),
        Field::new("duration_centuries", DataType::Int16, false),
        Field::new("duration_ns", DataType::UInt64, false),
    ]))
}

/// An efficient builder for creating [`RecordBatch`]es following the SpaceTimestamp schema.
///
/// This builder uses specialized Arrow builders for each column and supports
/// high-performance sequential appends.
pub struct SpaceTimestampBuilder {
    frame_id: StringDictionaryBuilder<UInt16Type>,
    units_pos: StringDictionaryBuilder<UInt16Type>,
    timescale_id: StringDictionaryBuilder<UInt16Type>,
    estimate_type: StringDictionaryBuilder<UInt16Type>,
    position: FixedSizeListBuilder<Float64Builder>,
    quaternion: FixedSizeListBuilder<Float64Builder>,
    duration_centuries: Int16Builder,
    duration_ns: UInt64Builder,
}

impl SpaceTimestampBuilder {
    /// Creates a new builder pre-allocated for the given capacity.
    ///
    /// # Arguments
    /// * `capacity` - The expected number of rows to be ingested before a flush.
    pub fn new(capacity: usize) -> Self {
        Self {
            frame_id: StringDictionaryBuilder::<UInt16Type>::with_capacity(capacity, 10, 100),
            units_pos: StringDictionaryBuilder::<UInt16Type>::with_capacity(capacity, 10, 100),
            timescale_id: StringDictionaryBuilder::<UInt16Type>::with_capacity(capacity, 10, 100),
            estimate_type: StringDictionaryBuilder::<UInt16Type>::with_capacity(capacity, 10, 100),
            position: FixedSizeListBuilder::new(Float64Builder::with_capacity(capacity * 3), 3),
            quaternion: FixedSizeListBuilder::new(Float64Builder::with_capacity(capacity * 4), 4),
            duration_centuries: Int16Builder::with_capacity(capacity),
            duration_ns: UInt64Builder::with_capacity(capacity),
        }
    }

    /// Returns the number of rows currently buffered in the builder.
    pub fn len(&self) -> usize {
        self.duration_ns.len()
    }

    /// Appends a single row of space-time data to the internal builders.
    ///
    /// # Arguments
    /// * `frame_id` - The name of the reference frame (e.g., "ICRF").
    /// * `units_pos` - The units for position coordinates (e.g., "m").
    /// * `timescale_id` - The timescale for the duration (e.g., "UTC").
    /// * `estimate_type` - The type of estimation (e.g., "PREDICTED").
    /// * `position` - An array of `[x, y, z]` coordinates.
    /// * `quaternion` - An array of `[w, x, y, z]` orientation components.
    /// * `duration_centuries` - The century component of the timestamp.
    /// * `duration_ns` - The nanosecond component of the timestamp.
    pub fn append_spacetimestamp(
        &mut self,
        frame_id: &str,
        units_pos: &str,
        timescale_id: &str,
        estimate_type: &str,
        position: [f64; 3],
        quaternion: [f64; 4],
        duration_centuries: i16,
        duration_ns: u64,
    ) {
        self.frame_id.append_value(frame_id);
        self.units_pos.append_value(units_pos);
        self.timescale_id.append_value(timescale_id);
        self.estimate_type.append_value(estimate_type);

        for p in position {
            self.position.values().append_value(p);
        }
        self.position.append(true);

        for q in quaternion {
            self.quaternion.values().append_value(q);
        }
        self.quaternion.append(true);

        self.duration_centuries.append_value(duration_centuries);
        self.duration_ns.append_value(duration_ns);
    }

    /// Consumes the buffered data and returns an Arrow [`RecordBatch`].
    ///
    /// This operation resets the internal builders, allowing them to be reused
    /// for the next batch of data.
    ///
    /// # Panics
    /// Panics if the generated columns do not match the provided `schema`.
    pub fn flush(&mut self, schema: SchemaRef) -> RecordBatch {
        RecordBatch::try_new(
            schema,
            vec![
                Arc::new(self.frame_id.finish()),
                Arc::new(self.units_pos.finish()),
                Arc::new(self.timescale_id.finish()),
                Arc::new(self.estimate_type.finish()),
                Arc::new(self.position.finish()),
                Arc::new(self.quaternion.finish()),
                Arc::new(self.duration_centuries.finish()),
                Arc::new(self.duration_ns.finish()),
            ],
        )
        .expect("should create record batch")
    }
}

/// Exports the [`sts_schema`] to an Arrow IPC file with 0 records.
///
/// This is useful for distributing the schema to other languages (Python, C++, etc.)
/// in a format they can natively understand.
pub fn export_sts_schema_to_file<P: AsRef<std::path::Path>>(
    path: P,
) -> Result<(), Box<dyn std::error::Error>> {
    let schema = sts_schema();
    let file = std::fs::File::create(path)?;
    let mut writer = arrow::ipc::writer::FileWriter::try_new(file, &schema)?;
    // We finish without adding any record batches to create a 0-record file.
    writer.finish()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use arrow::datatypes::DataType;

    use log::info;
    use test_log::test;

    #[test]
    fn test_schema_definition() {
        let s = sts_schema();
        assert_eq!(s.fields().len(), 8);

        let frame_field = s.field_with_name("frame_id").unwrap();
        match frame_field.data_type() {
            DataType::Dictionary(k, v) => {
                assert_eq!(**k, DataType::UInt16);
                assert_eq!(**v, DataType::Utf8);
            }
            _ => panic!("frame_id should be Dictionary(UInt16, Utf8)"),
        }

        info!("frame_field: {:?}", frame_field);

        let pos = s.field_with_name("position").unwrap();
        match pos.data_type() {
            DataType::FixedSizeList(f, size) => {
                assert_eq!(f.data_type(), &DataType::Float64);
                assert_eq!(*size, 3);
            }
            _ => panic!("position should be FixedSizeList(Float64, 3)"),
        }

        let q = s.field_with_name("quaternion").unwrap();
        match q.data_type() {
            DataType::FixedSizeList(f, size) => {
                assert_eq!(f.data_type(), &DataType::Float64);
                assert_eq!(*size, 4);
            }
            _ => panic!("quaternion should be FixedSizeList(Float64, 4)"),
        }

        let dur = s.field_with_name("duration_centuries").unwrap();
        assert_eq!(dur.data_type(), &DataType::Int16);

        let dur_ns = s.field_with_name("duration_ns").unwrap();
        assert_eq!(dur_ns.data_type(), &DataType::UInt64);
    }

    #[test]
    fn test_recordbatch_generation_and_sampling() {
        use arrow::array::{FixedSizeListArray, Float64Array};

        let s = sts_schema();
        let mut builder = SpaceTimestampBuilder::new(10);

        // Generate 10 rows
        for i in 0..10 {
            builder.append_spacetimestamp(
                "EME2000",
                "km",
                "TDB",
                "MEASURED",
                [i as f64, i as f64 * 10.0, i as f64 * 100.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
                i as u64,
            );
        }

        assert_eq!(builder.len(), 10);
        let batch = builder.flush(s);

        info!("Created batch with {} rows", batch.num_rows());
        assert_eq!(batch.num_rows(), 10);

        // Sample a subset
        let subset = batch.slice(2, 4); // Offset 2, length 4
        assert_eq!(subset.num_rows(), 4);

        // Verify data in subset
        let pos_col = subset
            .column_by_name("position")
            .unwrap()
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .unwrap();

        for i in 0..4 {
            // The value at index `i` in the subset corresponds to `i + 2` in the original data
            let list_val = pos_col.value(i);
            let pos_vals = list_val.as_any().downcast_ref::<Float64Array>().unwrap();
            assert_eq!(pos_vals.value(0), (i + 2) as f64);
        }
        info!("Subset verification successful");
    }

    #[test]
    fn test_schema_export_and_roundtrip() -> Result<(), Box<dyn std::error::Error>> {
        use arrow::ipc::reader::FileReader;
        use std::fs::File;

        // Create a temporary file path
        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join("sts_schema_test.arrow");

        // 1. Export the schema
        export_sts_schema_to_file(&file_path)?;

        // 2. Load the schema back from the file
        let file = File::open(&file_path)?;
        let reader = FileReader::try_new(file, None)?;
        let loaded_schema = reader.schema();

        // Verify the loaded schema matches our expectations
        assert_eq!(loaded_schema.fields().len(), 8);
        assert!(loaded_schema.field_with_name("frame_id").is_ok());

        // 3. Generate data using the loaded schema
        let mut builder = SpaceTimestampBuilder::new(5);
        for i in 0..5 {
            builder.append_spacetimestamp(
                "ICRF",
                "m",
                "UTC",
                "ESTIMATED",
                [i as f64; 3],
                [0.0, 0.0, 0.0, 1.0],
                0,
                i as u64,
            );
        }

        let batch = builder.flush(loaded_schema);

        // 4. Validate the batch
        assert_eq!(batch.num_rows(), 5);
        let timescale_col = batch.column_by_name("timescale_id").unwrap();
        assert_eq!(timescale_col.len(), 5);

        // Clean up
        let _ = std::fs::remove_file(file_path);

        Ok(())
    }
}
