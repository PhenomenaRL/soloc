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
    StringDictionaryBuilder, StructArray, UInt64Builder,
};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef, UInt16Type, UInt32Type};
use arrow::record_batch::RecordBatch;

/// The fixed name of the spacetimestamp struct column in any soloc schema.
///
/// All Arrow schemas that embed a spacetimestamp must use this exact column name.
/// The name is fixed (not configurable) so that validation, querying, and transforms
/// can locate the column without caller-supplied parameters.
pub const STS_COLUMN: &str = "spacetimestamp";

/// Returns `true` if `id` is a federated entity URI rather than an astronomical frame name.
///
/// Entity URIs contain a `:` separating the authority (`naif`, `norad`, `acme.com`, …) from
/// the local path. Astronomical frame names (`ICRF`, `IAU_EARTH`, `J2000`) never contain `:`.
///
/// Used by frame-resolution code to decide whether to look up `id` in the ledger (entity URI)
/// or pass it directly to the anise almanac (astronomical frame name).
pub fn is_entity_uri(id: &str) -> bool {
    id.contains(':')
}

/// Appends one nullable 6-element covariance entry to a `FixedSizeListBuilder`.
fn append_optional_cov6(
    builder: &mut FixedSizeListBuilder<Float64Builder>,
    value: Option<[f64; 6]>,
) {
    match value {
        Some(v) => {
            for x in v {
                builder.values().append_value(x);
            }
            builder.append(true);
        }
        None => {
            for _ in 0..6 {
                builder.values().append_null();
            }
            builder.append(false);
        }
    }
}

/// Returns the canonical Arrow schema for SpaceTimestamp data.
///
/// The schema carries no metadata: frame topology is derived from the rows themselves
/// (see [`crate::topology::TransformTree`]), not declared alongside the schema.
///
/// The schema consists of:
/// * `frame_id`: Dictionary-encoded reference frame (e.g., "ICRF").
/// * `units_pos`: Dictionary-encoded units for position (e.g., "km").
/// * `timescale_id`: Dictionary-encoded timescale (e.g., "TDB").
/// * `source_id`: Dictionary-encoded identifier of the source (e.g., a UUID or "sensor_1").
/// * `estimate_type`: Dictionary-encoded source of data (e.g., "MEASURED").
/// * `position`: `FixedSizeList(3, Float64)` containing `[x, y, z]`.
/// * `quaternion`: `FixedSizeList(4, Float64)` containing `[w, x, y, z]`.
/// * `duration_centuries`: Signed 16-bit integer for large time offsets.
/// * `duration_ns`: Unsigned 64-bit integer for nanosecond precision.
/// * `position_covariance`: Nullable `FixedSizeList(6, Float64)` — upper triangle of the
///   3×3 position covariance matrix, row-major: `[σ_xx, σ_xy, σ_xz, σ_yy, σ_yz, σ_zz]`.
///   Expressed in the same frame and units as `position`. Null when unknown.
/// * `orientation_covariance`: Nullable `FixedSizeList(6, Float64)` — upper triangle of the
///   3×3 orientation covariance in the tangent space of SO(3) (axis-angle perturbation),
///   row-major: `[σ_11, σ_12, σ_13, σ_22, σ_23, σ_33]`. Null when unknown.
pub fn sts_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new(
            "frame_id",
            DataType::Dictionary(Box::new(DataType::UInt32), Box::new(DataType::Utf8)),
            false,
        ),
        Field::new(
            "units_pos",
            DataType::Dictionary(Box::new(DataType::UInt16), Box::new(DataType::Utf8)),
            false,
        ),
        Field::new(
            "timescale_id",
            DataType::Dictionary(Box::new(DataType::UInt32), Box::new(DataType::Utf8)),
            false,
        ),
        Field::new(
            "source_id",
            DataType::Dictionary(Box::new(DataType::UInt32), Box::new(DataType::Utf8)),
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
        Field::new(
            "position_covariance",
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float64, true)), 6),
            true,
        ),
        Field::new(
            "orientation_covariance",
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float64, true)), 6),
            true,
        ),
    ]))
}

/// An efficient builder for creating [`RecordBatch`]es following the SpaceTimestamp schema.
///
/// `frame_id` and `source_id` are written exactly as supplied — the builder performs no
/// namespacing. Callers are responsible for passing fully-qualified identifiers, the same
/// convention `entity_id` already follows.
pub struct SpaceTimestampBuilder {
    frame_id: StringDictionaryBuilder<UInt32Type>,
    units_pos: StringDictionaryBuilder<UInt16Type>,
    timescale_id: StringDictionaryBuilder<UInt32Type>,
    source_id: StringDictionaryBuilder<UInt32Type>,
    estimate_type: StringDictionaryBuilder<UInt16Type>,
    position: FixedSizeListBuilder<Float64Builder>,
    quaternion: FixedSizeListBuilder<Float64Builder>,
    duration_centuries: Int16Builder,
    duration_ns: UInt64Builder,
    position_covariance: FixedSizeListBuilder<Float64Builder>,
    orientation_covariance: FixedSizeListBuilder<Float64Builder>,
}

impl SpaceTimestampBuilder {
    /// Creates a new builder pre-allocated for the given capacity.
    ///
    /// # Arguments
    /// * `capacity` - The expected number of rows to be ingested before a flush.
    pub fn new(capacity: usize) -> Self {
        Self {
            frame_id: StringDictionaryBuilder::<UInt32Type>::with_capacity(capacity, 10, 100),
            units_pos: StringDictionaryBuilder::<UInt16Type>::with_capacity(capacity, 10, 100),
            timescale_id: StringDictionaryBuilder::<UInt32Type>::with_capacity(capacity, 10, 100),
            source_id: StringDictionaryBuilder::<UInt32Type>::with_capacity(capacity, 10, 100),
            estimate_type: StringDictionaryBuilder::<UInt16Type>::with_capacity(capacity, 10, 100),
            position: FixedSizeListBuilder::new(Float64Builder::with_capacity(capacity * 3), 3),
            quaternion: FixedSizeListBuilder::new(Float64Builder::with_capacity(capacity * 4), 4),
            duration_centuries: Int16Builder::with_capacity(capacity),
            duration_ns: UInt64Builder::with_capacity(capacity),
            position_covariance: FixedSizeListBuilder::new(
                Float64Builder::with_capacity(capacity * 6),
                6,
            ),
            orientation_covariance: FixedSizeListBuilder::new(
                Float64Builder::with_capacity(capacity * 6),
                6,
            ),
        }
    }

    /// Returns the number of rows currently buffered in the builder.
    pub fn len(&self) -> usize {
        self.duration_ns.len()
    }

    /// Returns true if the builder contains no data.
    pub fn is_empty(&self) -> bool {
        self.duration_ns.is_empty()
    }

    /// Appends a single row of space-time data to the internal builders.
    ///
    /// `frame_id` and `source_id` are stored verbatim; supply fully-qualified identifiers.
    ///
    /// # Covariance convention
    ///
    /// Both covariance fields store the **upper triangle, row-major** of the corresponding
    /// 3×3 symmetric matrix as 6 values: `[σ_11, σ_12, σ_13, σ_22, σ_23, σ_33]`.
    /// This matches the CCSDS OPM/CDM convention. Pass `None` when uncertainty is unknown.
    #[allow(clippy::too_many_arguments)]
    pub fn append_spacetimestamp(
        &mut self,
        frame_id: &str,
        units_pos: &str,
        timescale_id: &str,
        source_id: &str,
        estimate_type: &str,
        position: [f64; 3],
        quaternion: [f64; 4],
        duration_centuries: i16,
        duration_ns: u64,
        position_covariance: Option<[f64; 6]>,
        orientation_covariance: Option<[f64; 6]>,
    ) {
        self.frame_id.append_value(frame_id);
        self.units_pos.append_value(units_pos);
        self.timescale_id.append_value(timescale_id);
        self.source_id.append_value(source_id);
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

        append_optional_cov6(&mut self.position_covariance, position_covariance);
        append_optional_cov6(&mut self.orientation_covariance, orientation_covariance);
    }

    /// Consumes the buffered data and returns an Arrow [`RecordBatch`].
    ///
    /// This automatically builds the canonical schema and packages the arrays.
    pub fn flush(&mut self) -> RecordBatch {
        let schema = sts_schema();
        RecordBatch::try_new(
            schema,
            vec![
                Arc::new(self.frame_id.finish()),
                Arc::new(self.units_pos.finish()),
                Arc::new(self.timescale_id.finish()),
                Arc::new(self.source_id.finish()),
                Arc::new(self.estimate_type.finish()),
                Arc::new(self.position.finish()),
                Arc::new(self.quaternion.finish()),
                Arc::new(self.duration_centuries.finish()),
                Arc::new(self.duration_ns.finish()),
                Arc::new(self.position_covariance.finish()),
                Arc::new(self.orientation_covariance.finish()),
            ],
        )
        .expect("should create record batch")
    }

    /// Consumes the buffered data and returns a [`StructArray`].
    ///
    /// This is useful for embedding the SpaceTimestamp data as a single nested
    /// column within a larger Arrow schema.
    pub fn finish_as_struct(&mut self) -> StructArray {
        let schema = sts_schema();
        let fields = schema.fields().clone();
        let arrays: Vec<Arc<dyn Array>> = vec![
            Arc::new(self.frame_id.finish()),
            Arc::new(self.units_pos.finish()),
            Arc::new(self.timescale_id.finish()),
            Arc::new(self.source_id.finish()),
            Arc::new(self.estimate_type.finish()),
            Arc::new(self.position.finish()),
            Arc::new(self.quaternion.finish()),
            Arc::new(self.duration_centuries.finish()),
            Arc::new(self.duration_ns.finish()),
            Arc::new(self.position_covariance.finish()),
            Arc::new(self.orientation_covariance.finish()),
        ];
        StructArray::try_new(fields, arrays, None).expect("should create struct array")
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
    fn test_benchmark_ingestion() {
        use std::time::Instant;

        let start = Instant::now();
        let num_records = 100_000;
        let mut builder = SpaceTimestampBuilder::new(num_records);

        for i in 0..num_records {
            builder.append_spacetimestamp(
                "ICRF",
                "m",
                "TAI",
                "sensor_1",
                "MEASURED",
                [i as f64, i as f64 * 2.0, i as f64 * 3.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
                i as u64,
                None,
                None,
            );
        }

        let batch = builder.flush();
        let duration = start.elapsed();

        info!(
            "Ingested {} records in {:?} ({:.2} records/sec)",
            num_records,
            duration,
            num_records as f64 / duration.as_secs_f64()
        );

        assert_eq!(batch.num_rows(), num_records);
    }

    #[test]
    fn test_schema_definition() {
        let s = sts_schema();
        assert_eq!(s.fields().len(), 11);

        let frame_field = s.field_with_name("frame_id").unwrap();
        match frame_field.data_type() {
            DataType::Dictionary(k, v) => {
                assert_eq!(**k, DataType::UInt32);
                assert_eq!(**v, DataType::Utf8);
            }
            _ => panic!("frame_id should be Dictionary(UInt32, Utf8)"),
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

        let mut builder = SpaceTimestampBuilder::new(10);

        // Generate 10 rows
        for i in 0..10 {
            builder.append_spacetimestamp(
                "EME2000",
                "km",
                "TDB",
                "sensor_1",
                "MEASURED",
                [i as f64, i as f64 * 10.0, i as f64 * 100.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
                i as u64,
                None,
                None,
            );
        }

        assert_eq!(builder.len(), 10);
        let batch = builder.flush();

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
        assert_eq!(loaded_schema.fields().len(), 11);
        assert!(loaded_schema.field_with_name("frame_id").is_ok());

        // 3. Generate data using the loaded schema
        let mut builder = SpaceTimestampBuilder::new(5);
        for i in 0..5 {
            builder.append_spacetimestamp(
                "ICRF",
                "m",
                "UTC",
                "sensor_1",
                "ESTIMATED",
                [i as f64; 3],
                [0.0, 0.0, 0.0, 1.0],
                0,
                i as u64,
                None,
                None,
            );
        }

        let batch = builder.flush();

        // 4. Validate the batch
        assert_eq!(batch.num_rows(), 5);
        let timescale_col = batch.column_by_name("timescale_id").unwrap();
        assert_eq!(timescale_col.len(), 5);

        // Clean up
        let _ = std::fs::remove_file(file_path);

        Ok(())
    }
}
