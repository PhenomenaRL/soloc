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
use arrow::datatypes::{DataType, Field, Schema, SchemaRef, UInt32Type};
use arrow::record_batch::RecordBatch;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// The metadata key used to store the serialized `FrameRegistry` in the Arrow schema.
pub const STS_REGISTRY_METADATA_KEY: &str = "soloc.frame_registry";

/// Represents a static spatial transformation between a child frame and its parent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FrameTransform {
    /// The ID of the parent frame this transform connects to.
    pub parent_id: String,
    /// The translation vector `[x, y, z]` relative to the parent frame.
    pub translation: [f64; 3],
    /// The rotation quaternion `[w, x, y, z]` relative to the parent frame.
    pub rotation_quat: [f64; 4],
}

/// A registry defining the static relationships between custom frames and root astronomical frames.
///
/// This registry is embedded as JSON into the Arrow schema metadata. It ensures that custom
/// frames (like "arm" or "base_link") are namespaced to avoid global collisions and that
/// the transformation tree is acyclic and anchors to a known astronomical root.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameRegistry {
    /// The unique namespace prefix for this registry (usually a UUID).
    pub namespace: String,
    /// Maps a fully qualified frame ID (`namespace:local_name`) to its transform definition.
    pub frames: HashMap<String, FrameTransform>,
}

impl Default for FrameRegistry {
    fn default() -> Self {
        Self::new_with_uuid()
    }
}

impl FrameRegistry {
    /// Creates a new registry with a randomly generated UUID v4 namespace.
    pub fn new_with_uuid() -> Self {
        let namespace = uuid::Uuid::new_v4().to_string();
        Self {
            namespace,
            frames: HashMap::new(),
        }
    }

    /// Creates a new registry with a specific namespace.
    pub fn new_with_namespace(namespace: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            frames: HashMap::new(),
        }
    }

    /// Returns the fully qualified, namespaced frame ID for a given local name.
    pub fn qualify(&self, local_name: &str) -> String {
        format!("{}:{}", self.namespace, local_name)
    }

    /// Adds a custom frame to the registry.
    ///
    /// The `local_name` is the name of the new frame.
    /// The `parent_name` can be either:
    /// 1. Another local name within this registry (e.g., "base_link").
    /// 2. An external astronomical frame (e.g., "MARS_IAU" or "ICRF").
    pub fn add_frame(
        &mut self,
        local_name: &str,
        parent_name: &str,
        translation: [f64; 3],
        rotation_quat: [f64; 4],
    ) {
        let child_id = self.qualify(local_name);

        // If the parent already contains a ':' or matches standard anise frame
        // conventions, we assume it's external or fully qualified.
        // Otherwise, we qualify it to our local namespace.
        let parent_id = if parent_name.contains(':')
            || parent_name == "ICRF"
            || parent_name.ends_with("_IAU")
        {
            parent_name.to_string()
        } else {
            self.qualify(parent_name)
        };

        self.frames.insert(
            child_id,
            FrameTransform {
                parent_id,
                translation,
                rotation_quat,
            },
        );
    }

    /// Validates the transform tree to ensure there are no cycles.
    /// Returns `Ok(())` if valid, or an `Err(String)` with the validation failure reason.
    pub fn validate(&self) -> Result<(), String> {
        for start_node in self.frames.keys() {
            let mut visited = HashSet::new();
            let mut current = start_node.clone();

            loop {
                if !visited.insert(current.clone()) {
                    return Err(format!("Cycle detected involving frame: {}", current));
                }

                match self.frames.get(&current) {
                    Some(transform) => {
                        current = transform.parent_id.clone();
                    }
                    None => {
                        // We reached a node not in our registry. This is our Root Anchor.
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    /// Serializes the registry to a JSON string.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Deserializes the registry from a JSON string.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }
}

/// Returns the canonical Arrow schema for SpaceTimestamp data.
///
/// If a `FrameRegistry` is provided, it is serialized and embedded into the
/// schema metadata under the `"soloc.frame_registry"` key.
///
/// The schema consists of:
/// * `frame_id`: Dictionary-encoded reference frame (e.g., "ICRF").
/// * `units_pos`: Dictionary-encoded units for position (e.g., "km").
/// * `timescale_id`: Dictionary-encoded timescale (e.g., "TDB").
/// * `estimate_type`: Dictionary-encoded source of data (e.g., "MEASURED").
/// * `position`: `FixedSizeList(3, Float64)` containing `[x, y, z]`.
/// * `quaternion`: `FixedSizeList(4, Float64)` containing `[w, x, y, z]`.
/// * `duration_centuries`: Signed 16-bit integer for large time offsets.
/// * `duration_ns`: Unsigned 64-bit integer for nanosecond precision.
pub fn sts_schema(registry: Option<&FrameRegistry>) -> SchemaRef {
    let mut metadata = HashMap::new();
    if let Some(reg) = registry {
        if let Ok(json) = reg.to_json() {
            metadata.insert(STS_REGISTRY_METADATA_KEY.to_string(), json);
        }
    }

    Arc::new(
        Schema::new(vec![
            Field::new(
                "frame_id",
                DataType::Dictionary(Box::new(DataType::UInt32), Box::new(DataType::Utf8)),
                false,
            ),
            Field::new(
                "units_pos",
                DataType::Dictionary(Box::new(DataType::UInt32), Box::new(DataType::Utf8)),
                false,
            ),
            Field::new(
                "timescale_id",
                DataType::Dictionary(Box::new(DataType::UInt32), Box::new(DataType::Utf8)),
                false,
            ),
            Field::new(
                "estimate_type",
                DataType::Dictionary(Box::new(DataType::UInt32), Box::new(DataType::Utf8)),
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
        ])
        .with_metadata(metadata),
    )
}

/// An efficient builder for creating [`RecordBatch`]es following the SpaceTimestamp schema.
///
/// This builder supports injecting a `FrameRegistry`. Any appended frames that
/// match a local name in the registry will be automatically namespace-qualified.
pub struct SpaceTimestampBuilder {
    registry: Option<FrameRegistry>,
    frame_id: StringDictionaryBuilder<UInt32Type>,
    units_pos: StringDictionaryBuilder<UInt32Type>,
    timescale_id: StringDictionaryBuilder<UInt32Type>,
    estimate_type: StringDictionaryBuilder<UInt32Type>,
    position: FixedSizeListBuilder<Float64Builder>,
    quaternion: FixedSizeListBuilder<Float64Builder>,
    duration_centuries: Int16Builder,
    duration_ns: UInt64Builder,
}

impl SpaceTimestampBuilder {
    /// Creates a new builder pre-allocated for the given capacity, with an optional registry.
    ///
    /// # Arguments
    /// * `capacity` - The expected number of rows to be ingested before a flush.
    /// * `registry` - An optional `FrameRegistry` to embed into the generated schema.
    pub fn new(capacity: usize, registry: Option<FrameRegistry>) -> Self {
        Self {
            registry,
            frame_id: StringDictionaryBuilder::<UInt32Type>::with_capacity(capacity, 10, 100),
            units_pos: StringDictionaryBuilder::<UInt32Type>::with_capacity(capacity, 10, 100),
            timescale_id: StringDictionaryBuilder::<UInt32Type>::with_capacity(capacity, 10, 100),
            estimate_type: StringDictionaryBuilder::<UInt32Type>::with_capacity(capacity, 10, 100),
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

    /// Returns true if the builder contains no data.
    pub fn is_empty(&self) -> bool {
        self.duration_ns.is_empty()
    }

    /// Appends a single row of space-time data to the internal builders.
    ///
    /// If the provided `frame_id` matches a local name in the builder's `FrameRegistry`,
    /// it will automatically be qualified with the registry's namespace prefix.
    #[allow(clippy::too_many_arguments)]
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
        let final_frame_id = if let Some(ref reg) = self.registry {
            let qualified = reg.qualify(frame_id);
            if reg.frames.contains_key(&qualified) {
                qualified
            } else {
                frame_id.to_string()
            }
        } else {
            frame_id.to_string()
        };

        self.frame_id.append_value(&final_frame_id);
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
    /// This automatically builds the canonical schema (including the serialized
    /// `FrameRegistry` metadata) and packages the arrays.
    pub fn flush(&mut self) -> RecordBatch {
        let schema = sts_schema(self.registry.as_ref());
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

    /// Consumes the buffered data and returns a [`StructArray`].
    ///
    /// This is useful for embedding the SpaceTimestamp data as a single nested
    /// column within a larger Arrow schema.
    pub fn finish_as_struct(&mut self) -> StructArray {
        let schema = sts_schema(self.registry.as_ref());
        let fields = schema.fields().clone();
        let arrays: Vec<Arc<dyn Array>> = vec![
            Arc::new(self.frame_id.finish()),
            Arc::new(self.units_pos.finish()),
            Arc::new(self.timescale_id.finish()),
            Arc::new(self.estimate_type.finish()),
            Arc::new(self.position.finish()),
            Arc::new(self.quaternion.finish()),
            Arc::new(self.duration_centuries.finish()),
            Arc::new(self.duration_ns.finish()),
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
    let schema = sts_schema(None);
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
        let mut builder = SpaceTimestampBuilder::new(num_records, None);

        for i in 0..num_records {
            builder.append_spacetimestamp(
                "ICRF",
                "m",
                "TAI",
                "MEASURED",
                [i as f64, i as f64 * 2.0, i as f64 * 3.0],
                [1.0, 0.0, 0.0, 0.0],
                0,
                i as u64,
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
        let s = sts_schema(None);
        assert_eq!(s.fields().len(), 8);

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

        let mut builder = SpaceTimestampBuilder::new(10, None);

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
        assert_eq!(loaded_schema.fields().len(), 8);
        assert!(loaded_schema.field_with_name("frame_id").is_ok());

        // 3. Generate data using the loaded schema
        let mut builder = SpaceTimestampBuilder::new(5, None);
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

        let batch = builder.flush();

        // 4. Validate the batch
        assert_eq!(batch.num_rows(), 5);
        let timescale_col = batch.column_by_name("timescale_id").unwrap();
        assert_eq!(timescale_col.len(), 5);

        // Clean up
        let _ = std::fs::remove_file(file_path);

        Ok(())
    }

    #[test]
    fn test_frame_registry_validation() {
        let mut reg = FrameRegistry::new_with_namespace("test_ns");

        // Valid Tree: arm -> base_link -> MARS_IAU
        reg.add_frame(
            "base_link",
            "MARS_IAU",
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
        );
        reg.add_frame("arm", "base_link", [1.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0]);
        assert!(reg.validate().is_ok());

        // Introduce a cycle: base_link parent becomes arm
        reg.add_frame("base_link", "arm", [0.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0]);
        assert!(reg.validate().is_err());
    }

    #[test]
    fn test_schema_metadata_injection() {
        let mut reg = FrameRegistry::new_with_namespace("robot_1");
        reg.add_frame("cam", "ICRF", [0.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0]);

        let schema = sts_schema(Some(&reg));
        let metadata = schema.metadata();

        assert!(metadata.contains_key(STS_REGISTRY_METADATA_KEY));
        let json = metadata.get(STS_REGISTRY_METADATA_KEY).unwrap();

        // Ensure we can deserialize it back
        let recovered_reg = FrameRegistry::from_json(json).unwrap();
        assert_eq!(recovered_reg.namespace, "robot_1");
        assert!(recovered_reg.frames.contains_key("robot_1:cam"));
    }

    #[test]
    fn test_builder_auto_namespacing() {
        let mut reg = FrameRegistry::new_with_namespace("robot_1");
        reg.add_frame("cam", "ICRF", [0.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0]);

        let mut builder = SpaceTimestampBuilder::new(10, Some(reg));

        // Append using local name (should auto-namespace to "robot_1:cam")
        builder.append_spacetimestamp(
            "cam",
            "m",
            "TAI",
            "MEASURED",
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            0,
        );

        // Append using global/external name (should remain "ICRF")
        builder.append_spacetimestamp(
            "ICRF",
            "m",
            "TAI",
            "ESTIMATED",
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            0,
        );

        let batch = builder.flush();
        let frame_col = batch.column_by_name("frame_id").unwrap();
        let dict_array = frame_col
            .as_any()
            .downcast_ref::<arrow::array::DictionaryArray<UInt32Type>>()
            .unwrap();
        let values = dict_array
            .values()
            .as_any()
            .downcast_ref::<arrow::array::StringArray>()
            .unwrap();

        assert_eq!(values.value(dict_array.key(0).unwrap()), "robot_1:cam");
        assert_eq!(values.value(dict_array.key(1).unwrap()), "ICRF");
    }
}
