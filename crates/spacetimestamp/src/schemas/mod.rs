pub mod entity;

use arrow::datatypes::SchemaRef;

/// Marker trait for batched Arrow schemas that embed `spacetimestamp` schema.
///
/// Implementors define the full Arrow schema (which must embed a `spacetimestamp`
/// struct column).
///
/// The identity column indicates the name of the column that holds PrescribedIds for the
/// parent schema.[`crate::topology::TransformTree`]: topology is derived from
/// `(id, frame_id, epoch)` triples, so a schema with no identity column can
/// carry poses but not parenting.
///
/// # Example
///
/// ```
/// use spacetimestamp::schemas::{SpaceTimestampSchema, entity::EntitySchema};
///
/// let schema = EntitySchema::schema();
/// let id_column = EntitySchema::id_column();
/// assert_eq!(id_column, "entity_id");
/// ```
///
/// A `soloc::ledger::Ledger` can be constructed directly from an implementor via
/// `Ledger::for_schema::<EntitySchema>()`. (Plain text, not a doc link: `soloc` depends on
/// this crate, not the other way around.)
pub trait SpaceTimestampSchema {
    /// Returns the full Arrow schema, which must embed a `"spacetimestamp"` struct column.
    fn schema() -> SchemaRef;
    /// Name of the entity-identity column, or `""` when this schema has no identity column.
    fn id_column() -> &'static str {
        ""
    }
}
