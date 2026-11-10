//! One GPS fix and a multi-batch GPX journey through the public ledger APIs.
//!
//! cargo run -p soloc-ledger --features gpx --example import_gpx
//! Add an output path to save the retained entity rows as Arrow IPC. Visualization
//! needs the `export_gpx` example's Earth/frame anchors and segment provenance.

use std::error::Error;

use anise::prelude::{Almanac, Epoch};
use soloc_ledger::gpx::import_gpx_into_ledger;
use soloc_ledger::ledger::Ledger;
use spacetimestamp::gps::gpx::{
    GpxEvent, GpxHeightPolicy, GpxImportEvent, GpxImportOptions, GpxMapper, GpxMapping,
    GpxObjectId, GpxPointErrorPolicy, GpxReader, GpxTimePolicy, WaypointPolicy,
};
use spacetimestamp::gps::{GpsObservation, GpsOptions, GpsOrientation, gps_observation_to_batch};
use spacetimestamp::query::SpatiotemporalFilter;
use spacetimestamp::schema::StsColumns;
use spacetimestamp::schemas::entity::EntitySchema;
use spacetimestamp::{EstimateType, LengthUnit, PrescribedId};

// Synthetic positions, times, and ellipsoidal elevations, explicitly SIMULATED.
const JOURNEY: &[u8] = include_bytes!("../tests/fixtures/gpx/halifax.gpx");

fn main() -> Result<(), Box<dyn Error>> {
    if std::env::args_os().nth(2).is_some() {
        return Err("usage: import_gpx [output.arrows]".into());
    }
    let almanac = Almanac::default(); // ITRF93 ingestion needs no kernels.
    let mut gps = GpsOptions::new(
        PrescribedId::new("soloc.example", "halifax-vessel")?,
        PrescribedId::abstract_source("soloc.example", "synthetic-gpx")?,
    );
    gps.estimate_type = EstimateType::SIMULATED;
    println!("Entity {} from source {}", gps.entity_id, gps.source_id);
    let source = "halifax.gpx";
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
    ledger.register_name(gps.entity_id, "soloc.example", "halifax-vessel")?;
    ledger.register_name(gps.source_id, "soloc.example", "synthetic-gpx")?;
    let mut single_fix_options = gps;
    single_fix_options.source_id = PrescribedId::abstract_source("soloc.example", "synthetic-fix")?;
    ledger.register_name(
        single_fix_options.source_id,
        "soloc.example",
        "synthetic-fix",
    )?;
    ledger.append(gps_observation_to_batch(
        &GpsObservation {
            latitude_deg: 44.6486,
            longitude_deg: -63.5754,
            ellipsoidal_height_m: 9.0,
            epoch: Epoch::from_gregorian_utc(2025, 1, 1, 11, 59, 50, 0),
            orientation: GpsOrientation::LocalEnu,
        },
        &single_fix_options,
        &almanac,
    )?)?;
    println!(
        "Appended one simulated GPS fix from source {}",
        single_fix_options.source_id
    );
    let report = import_gpx_into_ledger(
        &mut ledger,
        GpxReader::new(JOURNEY, source, WaypointPolicy::Reject),
        &mapper,
        &almanac,
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
    if report.submitted != 5 || history.num_rows() != 6 {
        return Err(
            "the single fix and complete five-observation journey were not retained".into(),
        );
    }
    println!(
        "Retained all {} observations in ITRF93 metres / TAI",
        history.num_rows()
    );
    let latest = ledger.current_state(Some(&[gps.entity_id]), None)?;
    if latest != history.slice(5, 1) {
        return Err("current_state did not select the final GPX observation".into());
    }
    // Same-frame unit conversion works offline. GCRF/IAU_EARTH transformations
    // use this same API with an Almanac containing the required local kernels.
    let kilometres = ledger.transform(&latest, "ITRF93", LengthUnit::km, &almanac)?;
    println!(
        "Latest of {} retained observations: {:?} km in ITRF93",
        history.num_rows(),
        StsColumns::try_new(&kilometres)?.position_at(0)
    );
    if let Some(path) = std::env::args_os().nth(1) {
        let path = std::path::Path::new(&path);
        ledger.save_ipc(path)?;
        let reloaded = Ledger::load_ipc(path, "entity_id")?;
        if reloaded.query(&SpatiotemporalFilter::new())? != history
            || reloaded.current_state(Some(&[gps.entity_id]), None)? != latest
            || reloaded.export_names()? != ledger.export_names()?
        {
            return Err("IPC reload did not preserve the observations and names".into());
        }
        println!(
            "Saved and reloaded all six observations and names from {}",
            path.display()
        );
    }
    Ok(())
}
