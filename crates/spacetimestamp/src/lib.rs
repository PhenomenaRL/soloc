//! Space-time coordinate data structures and Arrow schema definitions.
//!
//! This crate provides the [`sts_schema`] for representing satellite or celestial
//! positions and orientations, along with the [`SpaceTimestampBuilder`] for
//! efficient, row-oriented ingestion of this data into Arrow [`RecordBatch`]es.

mod schema;
pub mod transforms;

// Re-export canonical schema items to the crate root for a clean API.
pub use crate::schema::{SpaceTimestampBuilder, export_sts_schema_to_file, sts_schema};
