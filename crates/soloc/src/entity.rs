use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use spacetimestamp::sts_schema;
use std::sync::Arc;

/// Returns a schema for an Entity that includes a nested SpaceTimestamp.
///
/// This schema demonstrates how to embed the canonical `SpaceTimestamp`
/// fields as a single nested `Struct` field named `pose`. This allows
/// for consistent querying across different entities that share the same
/// space-time coordinate structure.
pub fn entity_schema() -> SchemaRef {
    let sts = sts_schema();

    Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("name", DataType::Utf8, false),
        // Embed the sts_schema fields as a Struct
        Field::new(
            "spacetimestamp",
            DataType::Struct(sts.fields().clone()),
            false,
        ),
        Field::new("mass_kg", DataType::Float64, true),
    ]))
}
