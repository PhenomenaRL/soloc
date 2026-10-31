//! Bounded GPX ingestion through the normal entity ledger (`gpx` feature).
//!
//! The importer preserves the ledger's configured retention policy. Submitted
//! counts describe successful appends, not the number of rows still retained.
//! To keep a complete journey, explicitly select adequate capacity or disable
//! eviction with [`Ledger::set_memory_limit(None)`](Ledger::set_memory_limit).
//! Changing the limit later cannot restore observations already evicted.

use std::io::BufRead;

use anise::prelude::Almanac;
use spacetimestamp::gps::gpx::{
    GpxImportEvent, GpxImportOptions, GpxImportReport, GpxMapper, GpxReader, GpxSubmitError,
    import_gpx,
};

use crate::ledger::Ledger;

/// Earth-frame anchors and source-aware playback exports.
pub mod playback;

/// Stream GPX observations into an existing ledger in bounded Arrow batches.
///
/// Each successful [`Ledger::append`] is confirmed in the returned report. A
/// rejected batch leaves the ledger unchanged; earlier appends remain in place.
/// Reading or conversion failure leaves any pending converted rows unsubmitted.
/// The report identifies these portions separately. There is no whole-file
/// transaction, automatic retry, or duplicate suppression: importing the same
/// input again appends its rows again.
///
/// `observe` receives source events, mapped points and their assumptions, skipped
/// point errors, and batch outcomes without accumulating them in memory. Keep
/// source events and the input alongside the entity data when provenance or
/// segment boundaries matter; EntitySchema has no GPX metadata/segment columns.
/// An observer sees a mapped point before its batch is accepted. Use batch
/// outcomes to distinguish converted points from submitted observations.
///
/// The ledger's retention setting is never changed. Successful submission can
/// evict historical rows when a memory limit is configured; it does not guarantee
/// complete-route replay or durable persistence.
pub fn import_gpx_into_ledger<R, O>(
    ledger: &mut Ledger,
    reader: GpxReader<R>,
    mapper: &GpxMapper<'_>,
    almanac: &Almanac,
    options: GpxImportOptions,
    observe: O,
) -> GpxImportReport
where
    R: BufRead,
    O: FnMut(GpxImportEvent<'_>),
{
    import_gpx(
        reader,
        mapper,
        almanac,
        options,
        |batch| ledger.append(batch).map_err(GpxSubmitError::rejected),
        observe,
    )
}
