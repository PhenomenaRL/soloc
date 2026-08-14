//! Space-time coordinates for anything you can track: schema, topology, and physics.
//!
//! A *spacetimestamp* is one entity's pose at one instant, recorded in whatever reference
//! frame and units it was measured in — see [`sts_schema`] for the canonical Arrow layout
//! and [`SpaceTimestampBuilder`] for row-oriented ingestion. Every schema in [`schemas`]
//! embeds that struct alongside an identity column.
//!
//! This crate is self-contained: it covers the whole path from raw measurements to a
//! reprojected batch, with no storage layer involved.
//!
//! ```text
//! schemas::entity::EntityBuilder   build a batch in native frames and units
//!   -> validation::validate_spacetimestamp_batch   check frames and timescales
//!   -> topology::TransformTree::ingest_batch       derive who is parented to whom
//!   -> transforms::transform_batch                 reproject into an astronomical frame
//!   -> query::filter_batch                         filter in space and time
//! ```
//!
//! Frame topology is never declared through a side channel; it is *derived* from the rows,
//! because every row already says "entity X is at this pose relative to `frame_id`". A
//! [`topology::TransformTree`] gives you the structure of a frame chain
//! ([`resolve_chain`](topology::TransformTree::resolve_chain)) but never a pose value — you
//! supply those, from your own store or from
//! [`ephemeris::celestial_snapshot`] for solar system bodies.
//!
//! The `soloc` crate builds on this one, adding an append-only ledger that keeps the poses,
//! maintains the tree across appends, and federates it. `tests/standalone_workflow.rs`
//! demonstrates the same pipeline without it.

pub mod ephemeris;
pub mod query;
pub mod schema;
pub mod schemas;
pub mod topology;
pub mod transforms;
pub mod validation;

// Re-export canonical schema items to the crate root for a clean API.
pub use crate::ephemeris::{epoch_from_parts, epoch_to_parts, j2000_in_timescale, j2000_tai};
pub use crate::schema::{STS_COLUMN, SpaceTimestampBuilder, export_sts_schema_to_file, sts_schema};
pub use crate::transforms::normalize_batch_to_tai;
