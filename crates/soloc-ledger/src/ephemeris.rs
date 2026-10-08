//! Ledger-bound ephemeris helpers.
//!
//! The snapshot and orbit functions themselves live in [`spacetimestamp::ephemeris`]. They only
//! need an [`anise::almanac::Almanac`], so they are usable without a ledger. This module
//! re-exports them so `soloc_ledger::ephemeris::celestial_snapshot` and `celestial_orbits` keep
//! resolving for callers who reach for the ledger crate first.
//!
//! Appending a snapshot needs no wrapper: `ledger.append(celestial_snapshot(&almanac, &ids,
//! epoch)?)?` is the whole operation, where `ids` are the bodies' `(naif, naif)` astronomical
//! ids (e.g. `CelestialBody::ALL.iter().map(|b| b.entity_id())`).
//!
//! # Ephemeris data
//!
//! Positions come from the DE440/DE440s SPK files. Load them via `anise::MetaAlmanac::latest()`,
//! which downloads ~150 MB on first run and caches them in `~/.local/share/nyx-space/anise/`.

pub use spacetimestamp::ephemeris::{
    CelestialBody, celestial_orbit, celestial_orbits, celestial_snapshot, default_centre,
    orbit_schema,
};
