//! Space-time coordinate data structures and Arrow schema definitions.
//!
//! This crate provides the [`sts_schema`] for representing satellite or celestial
//! positions and orientations, along with the [`SpaceTimestampBuilder`] for
//! efficient, row-oriented ingestion of this data into Arrow [`RecordBatch`]es.

pub mod ephemeris;
pub mod query;
pub mod schema;
pub mod transforms;
pub mod validation;

// Re-export canonical schema items to the crate root for a clean API.
pub use crate::ephemeris::{epoch_from_parts, epoch_to_parts, j2000_in_timescale, j2000_tai};
pub use crate::schema::{STS_COLUMN, SpaceTimestampBuilder, export_sts_schema_to_file, sts_schema};
pub use crate::transforms::normalize_batch_to_tai;
