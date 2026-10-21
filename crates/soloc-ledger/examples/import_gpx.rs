//! A small, multi-batch GPX journey with complete local history retention.
//!
//! cargo run -p soloc-ledger --features gpx --example import_gpx
//! Add an output path to save the retained entity rows as Arrow IPC. Visualization
//! needs the `export_gpx` example's Earth/frame anchors and segment provenance.

use std::error::Error;

use anise::prelude::Almanac;
use soloc_ledger::gpx::import_gpx_into_ledger;
use soloc_ledger::ledger::Ledger;
use spacetimestamp::gps::gpx::{
    GpxEvent, GpxHeightPolicy, GpxImportEvent, GpxImportOptions, GpxMapper, GpxMapping,
    GpxObjectId, GpxPointErrorPolicy, GpxReader, GpxTimePolicy, WaypointPolicy,
};
use spacetimestamp::gps::{GpsOptions, GpsOrientation};
use spacetimestamp::query::SpatiotemporalFilter;
use spacetimestamp::schemas::entity::EntitySchema;
use spacetimestamp::{EstimateType, PrescribedId};

// Synthetic positions, times, and ellipsoidal elevations, explicitly SIMULATED.
const JOURNEY: &str = r#"<gpx xmlns="http://www.topografix.com/GPX/1/1" version="1.1" creator="soloc-example">
  <trk><name>Halifax example</name><trkseg>
    <trkpt lat="44.6488" lon="-63.5752"><ele>10</ele><time>2025-01-01T12:00:00Z</time></trkpt>
    <trkpt lat="44.6490" lon="-63.5750"><ele>11</ele><time>2025-01-01T12:00:10Z</time></trkpt>
    <trkpt lat="44.6492" lon="-63.5748"><ele>12</ele><time>2025-01-01T12:00:20Z</time></trkpt>
  </trkseg><trkseg>
    <trkpt lat="44.6500" lon="-63.5740"><ele>13</ele><time>2025-01-01T12:05:00Z</time></trkpt>
    <trkpt lat="44.6502" lon="-63.5738"><ele>14</ele><time>2025-01-01T12:05:10Z</time></trkpt>
  </trkseg></trk>
</gpx>"#;

fn main() -> Result<(), Box<dyn Error>> {
    let mut gps = GpsOptions::new(
        PrescribedId::new("soloc.example", "halifax-vessel")?,
        PrescribedId::abstract_source("soloc.example", "synthetic-gpx")?,
    );
    gps.estimate_type = EstimateType::SIMULATED;
    println!("Entity {} from source {}", gps.entity_id, gps.source_id);
    let source = "halifax-example.gpx";
    let mapper = GpxMapper::new(
        source,
        [(
            GpxObjectId::Track(1),
            GpxMapping {
                gps,
                height: GpxHeightPolicy::EllipsoidalElevation,
                time: GpxTimePolicy::RequireTimestamp,
                orientation: GpsOrientation::LocalEnu,
            },
        )],
    )?;
    let mut ledger = Ledger::for_schema::<EntitySchema>()?;
    // Make the full-history acceptance condition explicit. Bounded import batches
    // do not protect history when the ledger is configured to evict old rows.
    ledger.set_memory_limit(None)?;
    let report = import_gpx_into_ledger(
        &mut ledger,
        GpxReader::new(JOURNEY.as_bytes(), source, WaypointPolicy::Reject),
        &mapper,
        &Almanac::default(), // ITRF93 ingestion needs no kernel downloads.
        GpxImportOptions::new(2, GpxPointErrorPolicy::Strict)?,
        |event| match event {
            GpxImportEvent::Input(event) if !matches!(event, GpxEvent::Point(_)) => {
                println!("Source event: {event:?}");
            }
            GpxImportEvent::Mapped { point, mapping } => {
                println!(
                    "Mapped {}: {:?} in {}, height {:?}, {:?} time, orientation {:?}",
                    point.point.context,
                    mapping.gps.estimate_type,
                    mapping.gps.target_frame,
                    mapping.height,
                    point.time_origin,
                    mapping.orientation,
                );
            }
            GpxImportEvent::Submitted(batch) => {
                println!(
                    "Accepted {} rows: {} through {}",
                    batch.rows, batch.first, batch.last
                );
            }
            _ => {}
        },
    );
    println!("Import report: {report:#?}");
    if !report.complete {
        return Err("import stopped; inspect the report for accepted and unsubmitted rows".into());
    }
    let history = ledger.query(&SpatiotemporalFilter::new())?;
    if report.submitted != 5 || history.num_rows() != report.submitted {
        return Err("the complete five-observation journey was not retained".into());
    }
    println!(
        "Retained all {} observations in ITRF93 metres / TAI",
        history.num_rows()
    );
    if let Some(path) = std::env::args_os().nth(1) {
        ledger.save_ipc(std::path::Path::new(&path))?;
        println!(
            "Saved entity rows and name registry to {}",
            path.to_string_lossy()
        );
    }
    Ok(())
}
