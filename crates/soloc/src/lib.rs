pub mod ephemeris;
pub mod ledger;

pub use spacetimestamp;

/// Batch-level schemas live in `spacetimestamp` so they can be used without a ledger.
/// Re-exported here so `soloc::schemas::entity::…` keeps resolving.
pub use spacetimestamp::schemas;
