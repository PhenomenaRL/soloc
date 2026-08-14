pub mod entity;

use arrow::datatypes::SchemaRef;

/// Marker trait for a batch-level Arrow schema built around a `spacetimestamp` column.
///
/// Implementors define the full Arrow schema (which must embed a `spacetimestamp`
/// struct column) and the column names used for spatiotemporal indexing and entity
/// identity.
///
/// The identity column is what makes a batch usable with
/// [`crate::topology::TransformTree`]: topology is derived from `(id, frame_id, epoch)`
/// triples, so a schema with no identity column can carry poses but not parenting.
///
/// # Example
///
/// ```rust,ignore
/// use spacetimestamp::schemas::{SpaceTimestampSchema, entity::EntitySchema};
///
/// let schema = EntitySchema::schema();
/// let id_column = EntitySchema::id_column();
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
