pub mod entity;

use arrow::datatypes::SchemaRef;
use spacetimestamp::schema::FrameRegistry;

/// Marker trait for any Arrow schema that can back a [`crate::ledger::Ledger`].
///
/// Implementors define the full Arrow schema (which must embed a `spacetimestamp`
/// struct column) and the column names the ledger uses for spatiotemporal indexing
/// and entity identity.
///
/// # Example
///
/// ```rust,ignore
/// use soloc::schemas::{SolocSchema, entity::EntitySchema};
/// use soloc::ledger::Ledger;
///
/// let ledger = Ledger::for_schema::<EntitySchema>(None)?;
/// ```
pub trait SolocSchema {
    /// Returns the full Arrow schema, which must embed a `"spacetimestamp"` struct column.
    fn schema(registry: Option<&FrameRegistry>) -> SchemaRef;
    /// Name of the entity-identity column, or `""` when this schema has no identity column.
    fn id_column() -> &'static str {
        ""
    }
}
