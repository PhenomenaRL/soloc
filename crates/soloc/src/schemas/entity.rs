//! The standard entity schema — a reference implementation of [`SolocSchema`].
//!
//! An Entity is a tracked object in the solar system: a spacecraft, planet, robot,
//! or sensor. It embeds a `spacetimestamp` for its pose and adds optional kinematic
//! fields (`velocity`, `angular_velocity`, `acceleration`, `mass_kg`, `state_covariance`).
//!
//! This is the first-party schema bundled with `soloc`, but it is not hardcoded anywhere
//! in the ledger or server. Users can supply a different schema by implementing
//! [`SolocSchema`] and passing it to [`crate::ledger::Ledger::for_schema`].
//!
//! # Semantics: Target vs. Observer
//!
//! * **Target** (`entity_id`): The entity whose state is being described.
//! * **Observer** (`spacetimestamp.source_id`): The entity or system that generated
//!   the measurement or prediction.

extern crate alloc;

use alloc::boxed::Box;
use alloc::sync::Arc;
use arrow::array::{FixedSizeListBuilder, Float64Builder, StringDictionaryBuilder};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef, UInt32Type};
use arrow::record_batch::RecordBatch;
use spacetimestamp::schema::{FrameRegistry, SpaceTimestampBuilder, sts_schema};

use super::SolocSchema;

// ---------------------------------------------------------------------------
// Schema + trait impl
// ---------------------------------------------------------------------------

/// Unit struct that implements [`SolocSchema`] for the standard entity schema.
///
/// Pass this as the type parameter to [`crate::ledger::Ledger::for_schema`]:
///
/// ```rust,ignore
/// use soloc::schemas::entity::EntitySchema;
/// use soloc::ledger::Ledger;
///
/// let ledger = Ledger::for_schema::<EntitySchema>(None)?;
/// ```
pub struct EntitySchema;

impl SolocSchema for EntitySchema {
    fn schema(registry: Option<&FrameRegistry>) -> SchemaRef {
        entity_schema(registry)
    }
    fn id_column() -> &'static str {
        "entity_id"
    }
}

// ---------------------------------------------------------------------------
// Schema factory
// ---------------------------------------------------------------------------

/// Returns a schema for an Entity that includes a nested SpaceTimestamp.
///
/// This schema is designed for the "Universal Ledger", capable of representing:
/// 1. Static IoT Sensors (only `spacetimestamp` populated).
/// 2. Planets/Spacecraft (populate `velocity` and `mass_kg`).
/// 3. Drones/Robots (populate `velocity`, `angular_velocity`, and `acceleration`).
pub fn entity_schema(registry: Option<&FrameRegistry>) -> SchemaRef {
    let sts = sts_schema(registry);

    Arc::new(Schema::new(vec![
        // Federated entity ID (e.g., "nasa.gov:perseverance" or "naif:499" for Mars)
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
        // Full 6×6 state covariance over [x,y,z,vx,vy,vz] — upper triangle, row-major (21 values).
        // Null when unknown.
        Field::new(
            "state_covariance",
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float64, true)), 21),
            true,
        ),
    ]))
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// An efficient builder for creating [`RecordBatch`]es following the Entity schema.
pub struct EntityBuilder {
    registry: Option<FrameRegistry>,
    entity_id: StringDictionaryBuilder<UInt32Type>,
    sts_builder: SpaceTimestampBuilder,
    velocity: FixedSizeListBuilder<Float64Builder>,
    angular_velocity: FixedSizeListBuilder<Float64Builder>,
    acceleration: FixedSizeListBuilder<Float64Builder>,
    mass_kg: Float64Builder,
    state_covariance: FixedSizeListBuilder<Float64Builder>,
}

impl EntityBuilder {
    /// Creates a new EntityBuilder pre-allocated for the given capacity.
    pub fn new(capacity: usize, registry: Option<FrameRegistry>) -> Self {
        Self {
            registry: registry.clone(),
            entity_id: StringDictionaryBuilder::<UInt32Type>::with_capacity(capacity, 10, 100),
            sts_builder: SpaceTimestampBuilder::new(capacity, registry),
            velocity: FixedSizeListBuilder::new(Float64Builder::with_capacity(capacity * 3), 3),
            angular_velocity: FixedSizeListBuilder::new(
                Float64Builder::with_capacity(capacity * 3),
                3,
            ),
            acceleration: FixedSizeListBuilder::new(Float64Builder::with_capacity(capacity * 3), 3),
            mass_kg: Float64Builder::with_capacity(capacity),
            state_covariance: FixedSizeListBuilder::new(
                Float64Builder::with_capacity(capacity * 21),
                21,
            ),
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
    #[allow(clippy::too_many_arguments)]
    pub fn append_entity(
        &mut self,
        entity_id: &str,
        frame_id: &str,
        units_pos: &str,
        timescale_id: &str,
        source_id: &str,
        estimate_type: &str,
        position: [f64; 3],
        quaternion: [f64; 4],
        duration_centuries: i16,
        duration_ns: u64,
        velocity: Option<[f64; 3]>,
        angular_velocity: Option<[f64; 3]>,
        acceleration: Option<[f64; 3]>,
        mass_kg: Option<f64>,
        state_covariance: Option<[f64; 21]>,
    ) {
        self.entity_id.append_value(entity_id);

        if let Some(v) = velocity {
            for val in v { self.velocity.values().append_value(val); }
            self.velocity.append(true);
        } else {
            for _ in 0..3 { self.velocity.values().append_null(); }
            self.velocity.append(false);
        }

        if let Some(w) = angular_velocity {
            for val in w { self.angular_velocity.values().append_value(val); }
            self.angular_velocity.append(true);
        } else {
            for _ in 0..3 { self.angular_velocity.values().append_null(); }
            self.angular_velocity.append(false);
        }

        if let Some(a) = acceleration {
            for val in a { self.acceleration.values().append_value(val); }
            self.acceleration.append(true);
        } else {
            for _ in 0..3 { self.acceleration.values().append_null(); }
            self.acceleration.append(false);
        }

        self.mass_kg.append_option(mass_kg);

        match state_covariance {
            Some(cov) => {
                for val in cov { self.state_covariance.values().append_value(val); }
                self.state_covariance.append(true);
            }
            None => {
                for _ in 0..21 { self.state_covariance.values().append_null(); }
                self.state_covariance.append(false);
            }
        }

        self.sts_builder.append_spacetimestamp(
            frame_id, units_pos, timescale_id, source_id, estimate_type,
            position, quaternion, duration_centuries, duration_ns,
            None, None,
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
                Arc::new(self.state_covariance.finish()),
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
        assert_eq!(schema.fields().len(), 7);

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
    fn test_entity_schema_impl() {
        let schema = EntitySchema::schema(None);
        assert_eq!(EntitySchema::id_column(), "entity_id");
        assert!(schema.field_with_name("spacetimestamp").is_ok());
    }

    #[test]
    fn test_entity_builder_flush() {
        let mut builder = EntityBuilder::new(10, None);

        builder.append_entity(
            "naif:399", "ICRF", "km", "TDB", "naif:de440s", "PREDICTED",
            [1.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], 0, 0,
            Some([0.0, 29.8, 0.0]), None, None, Some(5.972e24), None,
        );
        builder.append_entity(
            "demo:sensor_1", "IAU_MARS", "m", "TAI", "demo:sensor_1", "MEASURED",
            [1.2, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], 0, 1000,
            None, None, None, None, None,
        );

        let batch = builder.flush();
        assert_eq!(batch.num_rows(), 2);
        assert_eq!(batch.column_by_name("velocity").unwrap().null_count(), 1);
        assert_eq!(batch.column_by_name("mass_kg").unwrap().null_count(), 1);
    }

    #[test]
    fn test_entity_batch_validation() {
        let mut builder = EntityBuilder::new(10, None);
        builder.append_entity(
            "naif:399", "ICRF", "km", "TDB", "naif:de440s", "PREDICTED",
            [1.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], 0, 0,
            Some([0.0, 29.8, 0.0]), None, None, Some(5.972e24), None,
        );
        let batch = builder.flush();

        let sts_col = batch.column_by_name("spacetimestamp").unwrap();
        let struct_array = sts_col.as_any().downcast_ref::<StructArray>().unwrap();
        let temp_batch =
            RecordBatch::try_new(sts_schema(None), struct_array.columns().to_vec()).unwrap();

        assert!(validate_spacetimestamp_batch(&temp_batch).is_ok());
    }
}
