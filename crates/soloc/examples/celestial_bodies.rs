//! Demonstrates building a Ledger of solar system body positions from the current time.
//!
//! Run with:
//!   cargo run --example celestial_bodies -p soloc
//!
//! Note: ~150 MB of ephemeris data is downloaded on first run and cached in
//!   ~/.local/share/nyx-space/anise/

use anise::prelude::MetaAlmanac;
use arrow::array::{Array, DictionaryArray, FixedSizeListArray, Float64Array, StringArray, StructArray};
use arrow::datatypes::UInt32Type;
use arrow::record_batch::RecordBatch;
use hifitime::{Duration, Epoch};

use soloc::ephemeris::{CelestialBody, append_celestial};
use soloc::ledger::Ledger;

fn main() -> Result<(), String> {
    // -------------------------------------------------------------------------
    // Load ephemeris.
    // MetaAlmanac::latest() downloads DE440s + PCK files on first run and
    // reuses the cached copies on every subsequent run.
    // -------------------------------------------------------------------------
    println!("Loading ephemeris (downloads ~150 MB on first run, then cached)...");
    let almanac = MetaAlmanac::latest()
        .map_err(|e| format!("Failed to load almanac: {e}"))?;
    println!("Ephemeris ready.\n");

    // -------------------------------------------------------------------------
    // Get the current system time as a hifitime Epoch.
    // -------------------------------------------------------------------------
    let now = Epoch::now().map_err(|e| format!("Failed to read system clock: {e}"))?;
    let one_minute = Duration::from_parts(0, 60_000_000_000u64); // 60 s in ns
    let one_minute_later = now + one_minute;

    println!("T+0  : {now}");
    println!("T+60s: {one_minute_later}\n");

    // -------------------------------------------------------------------------
    // Snapshot all supported bodies at the current epoch and append to ledger.
    // -------------------------------------------------------------------------
    let mut ledger = Ledger::new();

    append_celestial(&mut ledger, &almanac, CelestialBody::ALL, now)?;
    println!("Appended {} bodies at T+0:", CelestialBody::ALL.len());
    print_entities(ledger.latest_snapshot(None).as_ref());
    println!();

    // -------------------------------------------------------------------------
    // One minute later: append a fresh snapshot.
    // Planetary positions change measurably over a minute — Earth moves ~1 800 km.
    // -------------------------------------------------------------------------
    append_celestial(&mut ledger, &almanac, CelestialBody::ALL, one_minute_later)?;
    println!("Appended {} bodies at T+60s:", CelestialBody::ALL.len());
    print_entities(ledger.latest_snapshot(None).as_ref());
    println!();

    println!("Ledger total: {} batches, {} rows each", ledger.len(), CelestialBody::ALL.len());

    Ok(())
}

/// Prints entity ID and SSB distance for every row in a snapshot batch.
fn print_entities(batch: Option<&RecordBatch>) {
    let Some(batch) = batch else { return };

    let Some(dict_col) = batch.column_by_name("entity_id") else { return };
    let Some(dict) = dict_col.as_any().downcast_ref::<DictionaryArray<UInt32Type>>() else { return };
    let Some(vals) = dict.values().as_any().downcast_ref::<StringArray>() else { return };

    let Some(sts) = batch.column_by_name("spacetimestamp")
        .and_then(|c| c.as_any().downcast_ref::<StructArray>()) else { return };
    let Some(pos_list) = sts.column_by_name("position")
        .and_then(|c| c.as_any().downcast_ref::<FixedSizeListArray>()) else { return };
    let Some(pos_vals) = pos_list.values().as_any().downcast_ref::<Float64Array>() else { return };

    for i in 0..batch.num_rows() {
        let full_id = vals.value(dict.keys().value(i) as usize);
        let name = full_id.split(':').last().unwrap_or(full_id);

        let base = (pos_list.offset() + i) * 3;
        let dist = {
            let x = pos_vals.value(base);
            let y = pos_vals.value(base + 1);
            let z = pos_vals.value(base + 2);
            (x * x + y * y + z * z).sqrt()
        };
        let dist_au = dist / 149_597_870.7;

        println!("  {name:<10}  {dist_au:>8.4} AU  ({dist:.0} km from SSB)");
    }
}
