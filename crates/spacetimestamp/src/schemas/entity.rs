//! The standard entity schema. A Reference implementation of [`SpaceTimestampSchema`].
//!
//! An Entity is a tracked object in the solar system: a spacecraft, planet, robot,
//! or sensor. It embeds a `spacetimestamp` for its pose and adds optional kinematic and
//! physical-property fields (`velocity`, `angular_velocity`, `acceleration`, `mass_kg`,
//! `state_covariance`, `dimensions`).
//!
//! This is the first-party schema bundled with this crate, but it is not hardcoded
//! anywhere in the ledger or server. Users can supply a different schema by implementing
//! [`SpaceTimestampSchema`].
//!
//! # Semantics: Target vs. Observer
//!
//! * **Target** (`entity_id`): The entity whose state is being described.
//! * **Observer** (`spacetimestamp.source_id`): The entity or system that generated
//!   the measurement or prediction.

extern crate alloc;

use crate::identity::{PrescribedId, id_builder, id_field};
use crate::schema::{SpaceTimestampBuilder, append_optional_list, sts_schema};
use crate::vocabulary::{EstimateType, LengthUnit, TimeScaleCode};
use alloc::sync::Arc;
use arrow::array::{FixedSizeBinaryBuilder, FixedSizeListBuilder, Float64Builder};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use arrow::record_batch::RecordBatch;

use super::SpaceTimestampSchema;

// ---------------------------------------------------------------------------
// Schema + trait impl
// ---------------------------------------------------------------------------

/// Unit struct that implements [`SpaceTimestampSchema`] for the standard entity schema.
pub struct EntitySchema;

impl SpaceTimestampSchema for EntitySchema {
    fn schema() -> SchemaRef {
        entity_schema()
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
///
/// `dimensions` defines bounding box in metres along the entity-local `[x, y, z]`
/// axes. Currently convention: body reference frame is geometrically centered,
/// and all entities have a rectangular bounding box centered on body reference frame
/// (ie. use x/2, y/2, z/2 in each direction from the center).
/// Will likely need to update/enforce this.
pub fn entity_schema() -> SchemaRef {
    let sts = sts_schema();

    Arc::new(Schema::new(vec![
        // Prescribed entity identity: minted.
        id_field("entity_id"),
        // Embed the sts_schema fields as a single Struct column
        Field::new(
            "spacetimestamp",
            DataType::Struct(sts.fields().clone()),
            false,
        ),
        // Linear velocity in m/s [vx, vy, vz]
        Field::new(
            "velocity",
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float64, true)), 3),
            true,
        ),
        // Angular velocity in rad/s [wx, wy, wz]
        Field::new(
            "angular_velocity",
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float64, true)), 3),
            true,
        ),
        // Linear acceleration in m/s^2 [ax, ay, az]
        Field::new(
            "acceleration",
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float64, true)), 3),
            true,
        ),
        // Physical mass
        Field::new("mass_kg", DataType::Float64, true),
        // Full 6×6 state covariance over [x,y,z,vx,vy,vz] — upper triangle, row-major (21 values).
        // Mixed units, following the blocks it covers: position in units_pos², velocity in
        // (m/s)², cross terms in units_pos·m/s. Null when unknown.
        Field::new(
            "state_covariance",
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float64, true)), 21),
            true,
        ),
        // Full physical dimensions in metres along the entity-local [x, y, z] axes.
        // Null when unknown.
        Field::new(
            "dimensions",
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float64, true)), 3),
            true,
        ),
    ]))
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// An efficient builder for creating [`RecordBatch`]es following the Entity schema.
pub struct EntityBuilder {
    entity_id: FixedSizeBinaryBuilder,
    sts_builder: SpaceTimestampBuilder,
    velocity: FixedSizeListBuilder<Float64Builder>,
    angular_velocity: FixedSizeListBuilder<Float64Builder>,
    acceleration: FixedSizeListBuilder<Float64Builder>,
    mass_kg: Float64Builder,
    state_covariance: FixedSizeListBuilder<Float64Builder>,
    dimensions: FixedSizeListBuilder<Float64Builder>,
}

impl EntityBuilder {
    /// Creates a new EntityBuilder pre-allocated for the given capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            entity_id: id_builder(capacity),
            sts_builder: SpaceTimestampBuilder::new(capacity),
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
            dimensions: FixedSizeListBuilder::new(Float64Builder::with_capacity(capacity * 3), 3),
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
        entity_id: PrescribedId,
        frame_id: PrescribedId,
        units_pos: LengthUnit,
        timescale_id: TimeScaleCode,
        source_id: PrescribedId,
        estimate_type: EstimateType,
        position: [f64; 3],
        quaternion: [f64; 4],
        duration_centuries: i16,
        duration_ns: u64,
        velocity_m_s: Option<[f64; 3]>,
        angular_velocity_rad_s: Option<[f64; 3]>,
        acceleration_m_s2: Option<[f64; 3]>,
        mass_kg: Option<f64>,
        state_covariance: Option<[f64; 21]>,
        dimensions_m: Option<[f64; 3]>,
    ) {
        self.entity_id
            .append_value(entity_id.as_bytes())
            .expect("PrescribedId is 16 bytes");

        append_optional_list(&mut self.velocity, velocity_m_s);
        append_optional_list(&mut self.angular_velocity, angular_velocity_rad_s);
        append_optional_list(&mut self.acceleration, acceleration_m_s2);

        self.mass_kg.append_option(mass_kg);

        append_optional_list(&mut self.state_covariance, state_covariance);
        append_optional_list(&mut self.dimensions, dimensions_m);

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
            None,
            None,
        );
    }

    /// Consumes the buffered data and returns an Arrow [`RecordBatch`].
    pub fn flush(&mut self) -> RecordBatch {
        let schema = entity_schema();
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
                Arc::new(self.dimensions.finish()),
            ],
        )
        .expect("should create record batch")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validation::validate_spacetimestamp_batch;
    use arrow::array::{Array, FixedSizeListArray, Float64Array, StructArray};

    /// An entity whose pose comes from ledger rows.
    fn entity(authority: &str, name: &str) -> PrescribedId {
        PrescribedId::new(authority, name).expect("test entity id should be valid")
    }

    /// A terminal astronomical frame id.
    fn frame(name: &str) -> PrescribedId {
        PrescribedId::astronomical_from_name(name).expect("test frame name should be valid")
    }

    #[test]
    fn test_entity_schema_definition() {
        let schema = entity_schema();
        assert_eq!(schema.fields().len(), 8);

        let entity_id = schema.field_with_name("entity_id").unwrap();
        assert_eq!(
            entity_id,
            &crate::identity::id_field("entity_id"),
            "entity_id must be built by id_field"
        );

        let sts = schema.field_with_name("spacetimestamp").unwrap();
        assert!(matches!(sts.data_type(), DataType::Struct(_)));

        let vel = schema.field_with_name("velocity").unwrap();
        assert!(vel.is_nullable());

        let dimensions = schema.field_with_name("dimensions").unwrap();
        assert!(dimensions.is_nullable());
        match dimensions.data_type() {
            DataType::FixedSizeList(item, size) => {
                assert_eq!(*size, 3);
                assert_eq!(item.data_type(), &DataType::Float64);
            }
            _ => panic!("dimensions should be FixedSizeList(3, Float64)"),
        }
    }

    #[test]
    fn test_entity_schema_impl() {
        let schema = EntitySchema::schema();
        assert_eq!(EntitySchema::id_column(), "entity_id");
        assert!(schema.field_with_name("spacetimestamp").is_ok());
    }

    #[test]
    fn test_entity_builder_flush() {
        let mut builder = EntityBuilder::new(10);

        builder.append_entity(
            entity("naif", "399"),
            frame("ICRF"),
            LengthUnit::km,
            TimeScaleCode::TDB,
            PrescribedId::abstract_source("naif", "de440s").unwrap(),
            EstimateType::ESTIMATED,
            [1.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            0,
            Some([0.0, 29.8, 0.0]),
            None,
            None,
            Some(5.972e24),
            None,
            Some([12_742_000.0, 12_742_000.0, 12_714_000.0]),
        );
        builder.append_entity(
            entity("demo", "sensor_1"),
            frame("IAU_MARS"),
            LengthUnit::m,
            TimeScaleCode::TAI,
            entity("demo", "sensor_1"),
            EstimateType::MEASURED,
            [1.2, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            1000,
            None,
            None,
            None,
            None,
            None,
            None,
        );

        let batch = builder.flush();
        assert_eq!(batch.num_rows(), 2);
        assert_eq!(batch.column_by_name("velocity").unwrap().null_count(), 1);
        assert_eq!(batch.column_by_name("mass_kg").unwrap().null_count(), 1);

        let dimensions = batch
            .column_by_name("dimensions")
            .unwrap()
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .unwrap();
        assert!(!dimensions.is_null(0));
        assert!(dimensions.is_null(1));
        let values = dimensions
            .values()
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap();
        assert_eq!(values.value(0), 12_742_000.0);
        assert_eq!(values.value(1), 12_742_000.0);
        assert_eq!(values.value(2), 12_714_000.0);
    }

    #[test]
    fn test_entity_batch_validation() {
        let mut builder = EntityBuilder::new(10);
        builder.append_entity(
            entity("naif", "399"),
            frame("ICRF"),
            LengthUnit::km,
            TimeScaleCode::TDB,
            PrescribedId::abstract_source("naif", "de440s").unwrap(),
            EstimateType::ESTIMATED,
            [1.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            0,
            0,
            Some([0.0, 29.8, 0.0]),
            None,
            None,
            Some(5.972e24),
            None,
            Some([12_742_000.0, 12_742_000.0, 12_714_000.0]),
        );
        let batch = builder.flush();

        let sts_col = batch.column_by_name("spacetimestamp").unwrap();
        let struct_array = sts_col.as_any().downcast_ref::<StructArray>().unwrap();
        let temp_batch =
            RecordBatch::try_new(sts_schema(), struct_array.columns().to_vec()).unwrap();

        assert!(validate_spacetimestamp_batch(&temp_batch).is_ok());
    }
}
