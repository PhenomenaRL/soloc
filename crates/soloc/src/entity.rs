//! Entity data structures and Arrow schema definitions.
//!
//! This module defines the canonical `entity_schema` for representing the state of
//! dynamic or static objects within the solar system (e.g., planets, robots, sensors).
//! It embeds the canonical [`sts_schema`] from the `spacetimestamp` crate.
//!
//! # Semantics: Target vs. Observer
//!
//! When generating `Entity` records, the distinction between the target and the
//! observer is critical:
//! * **Target** (`entity_id`): The entity whose state is being described (e.g., "spaceship_a").
//! * **Observer** (`spacetimestamp.source_id`): The entity or system that generated
//!   the measurement or prediction (e.g., "spaceship_a" or "telescope_b").
//!
//! **Self-Reporting (Telemetry):** When an entity reports its own state, `entity_id`
//! and `source_id` typically match or share the same root URI.
//!
//! **External Observation (Tracking):** When an entity (e.g., a telescope) tracks
//! another entity, `entity_id` is the target being tracked, while `source_id` is
//! the telescope. The `frame_id` will often be relative to the `source_id`.
//!
//! # Future Validation
//! In the future, the `soloc` ledger ingestion engine will enforce referential
//! integrity rules based on these semantics. For example:
//! 1. **Registered Observer Rule:** `source_id` must be a known, authenticated entity.
//! 2. **Contextual Validation:** If `entity_id != source_id`, `soloc` will verify that
//!    the provided `frame_id` exists within the `source_id`'s registered frame graph.

extern crate alloc;

use alloc::boxed::Box;
use alloc::sync::Arc;
use arrow::array::{FixedSizeListBuilder, Float64Builder, StringDictionaryBuilder};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef, UInt32Type};
use arrow::record_batch::RecordBatch;
use spacetimestamp::schema::{FrameRegistry, SpaceTimestampBuilder, sts_schema};

/// Returns a schema for an Entity that includes a nested SpaceTimestamp.
///
/// This schema is designed for the "Universal Ledger", capable of representing:
/// 1. Static IoT Sensors (only `spacetimestamp` populated).
/// 2. Planets/Spacecraft (populate `velocity` and `mass_kg`).
/// 3. Drones/Robots (populate `velocity`, `angular_velocity`, and `acceleration`).
pub fn entity_schema(registry: Option<&FrameRegistry>) -> SchemaRef {
    let sts = sts_schema(registry);

    Arc::new(Schema::new(vec![
        // Dictionary encoded string URI (e.g., "urn:soloc:nasa:perseverance")
        Field::new(
            "entity_id",
            DataType::Dictionary(Box::new(DataType::UInt32), Box::new(DataType::Utf8)),
            false,
        ),
        // Embed the sts_schema fields as a single Struct column
        Field::new(
            "spacetimestamp",
            DataType::Struct(sts.fields().clone()),
            false,
        ),
        // Linear Velocity (e.g., km/s or m/s) [vx, vy, vz]
        Field::new(
            "velocity",
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float64, true)), 3),
            true,
        ),
        // Angular velocity (e.g., rad/s) [wx, wy, wz]
        Field::new(
            "angular_velocity",
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float64, true)), 3),
            true,
        ),
        // Linear Acceleration (e.g., m/s^2) [ax, ay, az]
        Field::new(
            "acceleration",
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float64, true)), 3),
            true,
        ),
        // Physical mass
        Field::new("mass_kg", DataType::Float64, true),
    ]))
}

/// An efficient builder for creating [`RecordBatch`]es following the Entity schema.
///
/// This builder wraps a [`SpaceTimestampBuilder`] to handle the complex, nested
/// spatial/temporal arrays, while providing its own builders for the top-level
/// Entity properties like `entity_id` and `velocity`.
pub struct EntityBuilder {
    registry: Option<FrameRegistry>,
    entity_id: StringDictionaryBuilder<UInt32Type>,
    sts_builder: SpaceTimestampBuilder,
    velocity: FixedSizeListBuilder<Float64Builder>,
    angular_velocity: FixedSizeListBuilder<Float64Builder>,
    acceleration: FixedSizeListBuilder<Float64Builder>,
    mass_kg: Float64Builder,
}

impl EntityBuilder {
    /// Creates a new EntityBuilder pre-allocated for the given capacity.
    ///
    /// # Arguments
    /// * `capacity` - The expected number of rows to be ingested before a flush.
    /// * `registry` - An optional `FrameRegistry` to embed into the generated schema.
    pub fn new(capacity: usize, registry: Option<FrameRegistry>) -> Self {
        Self {
            registry: registry.clone(),
            entity_id: StringDictionaryBuilder::<UInt32Type>::with_capacity(capacity, 10, 100),
            // Pass the cloned registry into the embedded builder
            sts_builder: SpaceTimestampBuilder::new(capacity, registry),
            velocity: FixedSizeListBuilder::new(Float64Builder::with_capacity(capacity * 3), 3),
            angular_velocity: FixedSizeListBuilder::new(
                Float64Builder::with_capacity(capacity * 3),
                3,
            ),
            acceleration: FixedSizeListBuilder::new(Float64Builder::with_capacity(capacity * 3), 3),
            mass_kg: Float64Builder::with_capacity(capacity),
        }
    }

    /// Returns the number of rows currently buffered in the builder.
    pub fn len(&self) -> usize {
        self.sts_builder.len()
    }

    /// Returns true if the builder contains no data.
    pub fn is_empty(&self) -> bool {
        self.sts_builder.is_empty()
    }

    /// Appends a single row of Entity data to the internal builders.
    ///
    /// # Arguments
    /// * `entity_id` - The unique URI for the target entity (e.g., "urn:soloc:spaceship_a").
    /// * `frame_id` - The reference frame for the pose/velocity.
    /// * `units_pos` - Units for position/velocity/acceleration.
    /// * `timescale_id` - Timescale (e.g., "TAI").
    /// * `source_id` - The observer or originator of the data (e.g., "urn:soloc:spaceship_a" or "urn:soloc:telescope_b").
    /// * `estimate_type` - Measurement type (e.g., "MEASURED" or "SIMULATED").
    /// * `position` - `[x, y, z]` coordinates.
    /// * `quaternion` - `[w, x, y, z]` orientation.
    /// * `duration_centuries` - Century component of the timestamp.
    /// * `duration_ns` - Nanosecond component of the timestamp.
    /// * `velocity` - Optional `[vx, vy, vz]`.
    /// * `angular_velocity` - Optional `[wx, wy, wz]`.
    /// * `acceleration` - Optional `[ax, ay, az]`.
    /// * `mass_kg` - Optional physical mass.
    #[allow(clippy::too_many_arguments)]
    pub fn append_entity(
        &mut self,
        entity_id: &str,
        // Spacetimestamp arguments
        frame_id: &str,
        units_pos: &str,
        timescale_id: &str,
        source_id: &str,
        estimate_type: &str,
        position: [f64; 3],
        quaternion: [f64; 4],
        duration_centuries: i16,
        duration_ns: u64,
        // Optional Entity properties
        velocity: Option<[f64; 3]>,
        angular_velocity: Option<[f64; 3]>,
        acceleration: Option<[f64; 3]>,
        mass_kg: Option<f64>,
    ) {
        // Top-level properties
        self.entity_id.append_value(entity_id);

        if let Some(v) = velocity {
            for val in v {
                self.velocity.values().append_value(val);
            }
            self.velocity.append(true);
        } else {
            for _ in 0..3 {
                self.velocity.values().append_null();
            }
            self.velocity.append(false);
        }

        if let Some(w) = angular_velocity {
            for val in w {
                self.angular_velocity.values().append_value(val);
            }
            self.angular_velocity.append(true);
        } else {
            for _ in 0..3 {
                self.angular_velocity.values().append_null();
            }
            self.angular_velocity.append(false);
        }

        if let Some(a) = acceleration {
            for val in a {
                self.acceleration.values().append_value(val);
            }
            self.acceleration.append(true);
        } else {
            for _ in 0..3 {
                self.acceleration.values().append_null();
            }
            self.acceleration.append(false);
        }

        self.mass_kg.append_option(mass_kg);

        // Delegate nested spacetimestamp properties
        self.sts_builder.append_spacetimestamp(
            frame_id,
            units_pos,
            timescale_id,
            source_id,
            estimate_type,
            position,
            quaternion,
            duration_centuries,
            duration_ns,
        );
    }

    /// Consumes the buffered data and returns an Arrow [`RecordBatch`].
    pub fn flush(&mut self) -> RecordBatch {
        let schema = entity_schema(self.registry.as_ref());
        let sts_struct_array = self.sts_builder.finish_as_struct();

        RecordBatch::try_new(
            schema,
            vec![
                Arc::new(self.entity_id.finish()),
                Arc::new(sts_struct_array),
                Arc::new(self.velocity.finish()),
                Arc::new(self.angular_velocity.finish()),
                Arc::new(self.acceleration.finish()),
                Arc::new(self.mass_kg.finish()),
            ],
        )
        .expect("should create record batch")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::StructArray;
    use spacetimestamp::validation::validate_spacetimestamp_batch;

    #[test]
    fn test_entity_schema_definition() {
        let schema = entity_schema(None);
        assert_eq!(schema.fields().len(), 6);

        let entity_id = schema.field_with_name("entity_id").unwrap();
        match entity_id.data_type() {
            DataType::Dictionary(k, v) => {
                assert_eq!(**k, DataType::UInt32);
                assert_eq!(**v, DataType::Utf8);
            }
            _ => panic!("entity_id should be Dictionary(UInt32, Utf8)"),
        }

        let sts = schema.field_with_name("spacetimestamp").unwrap();
        assert!(matches!(sts.data_type(), DataType::Struct(_)));

        let vel = schema.field_with_name("velocity").unwrap();
        assert!(vel.is_nullable());
    }

    #[test]
    fn test_entity_builder_flush() {
        let mut builder = EntityBuilder::new(10, None);

        // Row 1: Planet (has velocity, no acceleration)
        builder.append_entity(
            "urn:soloc:earth",
            "ICRF",
            "km",
            "TDB",
            "anise_ephemeris",
            "PREDICTED",
            [1.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            0,
            Some([0.0, 29.8, 0.0]),
            None,
            None,
            Some(5.972e24),
        );

        // Row 2: IoT Sensor (only has pose, everything else null)
        builder.append_entity(
            "urn:soloc:sensor_1",
            "IAU_MARS",
            "m",
            "TAI",
            "sensor_1",
            "MEASURED",
            [1.2, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            1000,
            None,
            None,
            None,
            None,
        );

        let batch = builder.flush();
        assert_eq!(batch.num_rows(), 2);

        let entity_col = batch.column_by_name("entity_id").unwrap();
        assert_eq!(entity_col.null_count(), 0);

        let vel_col = batch.column_by_name("velocity").unwrap();
        assert_eq!(vel_col.null_count(), 1); // Row 2 has no velocity

        let mass_col = batch.column_by_name("mass_kg").unwrap();
        assert_eq!(mass_col.null_count(), 1); // Row 2 has no mass
    }

    #[test]
    fn test_entity_batch_validation() {
        let mut builder = EntityBuilder::new(10, None);
        builder.append_entity(
            "urn:soloc:earth",
            "ICRF",
            "km",
            "TDB",
            "anise",
            "PREDICTED",
            [1.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            0,
            Some([0.0, 29.8, 0.0]),
            None,
            None,
            Some(5.972e24),
        );
        let batch = builder.flush();

        // Extract the nested struct column and repackage it as a temporary batch
        // to pass to our agnostic validation function.
        let sts_col = batch.column_by_name("spacetimestamp").unwrap();
        let struct_array = sts_col.as_any().downcast_ref::<StructArray>().unwrap();
        let temp_batch =
            RecordBatch::try_new(sts_schema(None), struct_array.columns().to_vec()).unwrap();

        assert!(validate_spacetimestamp_batch(&temp_batch).is_ok());
    }
}
