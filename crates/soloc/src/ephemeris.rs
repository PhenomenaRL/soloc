//! Ledger-bound ephemeris helpers.
//!
//! The snapshot functions themselves live in [`spacetimestamp::ephemeris`] — they only need
//! an [`anise::almanac::Almanac`] and an entity builder, so they are usable without a ledger. This
//! module holds the two wrappers that genuinely need one, and re-exports the rest so
//! `soloc::ephemeris::celestial_snapshot` keeps resolving.
//!
//! # Ephemeris data
//!
//! Positions come from the DE440/DE440s SPK files. Load them via `anise::MetaAlmanac::latest()`,
//! which downloads ~150 MB on first run and caches them in `~/.local/share/nyx-space/anise/`.

use anise::prelude::Almanac;
use hifitime::Epoch;

use crate::ledger::Ledger;

pub use spacetimestamp::ephemeris::{CelestialBody, celestial_snapshot, naif_snapshot};

/// Convenience wrapper: calls [`naif_snapshot`] and appends the result to `ledger`.
pub fn append_naif(
    ledger: &mut Ledger,
    almanac: &Almanac,
    bodies: &[(i32, &str)],
    epoch: Epoch,
) -> Result<(), String> {
    let batch = naif_snapshot(almanac, bodies, epoch)?;
    ledger.append(batch)?;
    Ok(())
}

/// Convenience wrapper: calls [`celestial_snapshot`] and appends the result to `ledger`.
///
/// Equivalent to `ledger.append(celestial_snapshot(almanac, bodies, epoch)?)`.
/// Returns `Err` without modifying the ledger if the snapshot fails.
pub fn append_celestial(
    ledger: &mut Ledger,
    almanac: &Almanac,
    bodies: &[CelestialBody],
    epoch: Epoch,
) -> Result<(), String> {
    let batch = celestial_snapshot(almanac, bodies, epoch)?;
    ledger.append(batch)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use anise::prelude::Almanac;
    use spacetimestamp::ephemeris::j2000_tai;

    #[test]
    fn test_append_celestial_does_not_mutate_ledger_on_error() {
        let mut ledger =
            crate::ledger::Ledger::new(&crate::schemas::entity::entity_schema(), "entity_id")
                .unwrap();
        let almanac = Almanac::default();
        let epoch = j2000_tai();
        let result = append_celestial(&mut ledger, &almanac, &[CelestialBody::Earth], epoch);
        assert!(result.is_err());
        assert!(
            ledger.is_empty(),
            "ledger should be unchanged after a failed append"
        );
    }

    #[test]
    fn test_append_naif_does_not_mutate_ledger_on_error() {
        let mut ledger =
            crate::ledger::Ledger::new(&crate::schemas::entity::entity_schema(), "entity_id")
                .unwrap();
        let almanac = Almanac::default();
        let epoch = j2000_tai();
        let result = append_naif(&mut ledger, &almanac, &[(399, "naif:399")], epoch);
        assert!(result.is_err());
        assert!(
            ledger.is_empty(),
            "ledger should be unchanged after a failed append"
        );
    }
}
