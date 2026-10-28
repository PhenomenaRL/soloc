//! Convert one estimated Sedona GPS observation and export an Earth-view ledger.
//!
//! SOLOC_KERNEL_PATHS=/path/earth_latest_high_prec.bpc cargo run --offline --locked \
//!   -p soloc-ledger --example export_sedona -- GEOID_UNDULATION_METRES [output.arrows]
//!
//! The input elevation is approximately 1,326 m above mean sea level, not an
//! ellipsoidal GPS height. Supply an explicit geoid undulation N to approximate
//! ellipsoidal height as h = H + N; the program does not guess a vertical datum.
//! For this demonstration, treating the user's approximate MSL height as EGM96
//! orthometric height gives N = -24.53045742765352 m (bilinear interpolation of
//! /usr/share/proj/egm96_15.gtx). The original elevation's vertical datum is unknown.
//! The original photo time is unknown. This demonstration explicitly assumes
//! 2025-01-01 12:00:00 UTC, within the supplied Earth BPC's coverage.
//! No GPX journey, measured attitude, velocity, or exact photo location is inferred.

use std::{error::Error, path::PathBuf};

use anise::prelude::{Almanac, Epoch};
use soloc_ledger::ledger::Ledger;
use spacetimestamp::PrescribedId;
use spacetimestamp::gps::{
    GpsObservation, GpsOptions, GpsOrientation, convert_gps_observation, itrf93_frame_snapshot,
};
use spacetimestamp::identity::ASTRO_AUTHORITY;
use spacetimestamp::schemas::entity::{EntityBuilder, EntitySchema};

const LATITUDE_DEG: f64 = 34.8697;
const LONGITUDE_DEG: f64 = -111.7611;
const MSL_HEIGHT_M: f64 = 1_326.0;
const DEFAULT_OUTPUT: &str = "visualizer/public/data/sedona-single-gps.arrows";
const USAGE: &str = "usage: export_sedona GEOID_UNDULATION_METRES [output.arrows]";

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let geoid_undulation_m: f64 = args.next().ok_or(USAGE)?.parse()?;
    if !geoid_undulation_m.is_finite() {
        return Err("geoid undulation must be a finite number of metres".into());
    }
    let output = args
        .next()
        .map_or_else(|| PathBuf::from(DEFAULT_OUTPUT), PathBuf::from);
    if args.next().is_some()
        || !matches!(
            output.extension().and_then(|s| s.to_str()),
            Some("arrow" | "arrows")
        )
    {
        return Err(USAGE.into());
    }

    let paths = std::env::var_os("SOLOC_KERNEL_PATHS")
        .filter(|paths| !paths.is_empty())
        .ok_or("set SOLOC_KERNEL_PATHS to a local Earth .bpc covering 2025-01-01 12:00:00 UTC")?;
    let mut almanac = Almanac::default();
    for path in std::env::split_paths(&paths) {
        if !path.is_file() {
            return Err(format!("kernel is not a local file: {}", path.display()).into());
        }
        almanac = almanac.load(path.to_str().ok_or("kernel path must be UTF-8")?)?;
    }

    // These are stable caller-chosen identities, not hashes of latitude/time.
    let entity = PrescribedId::new("soloc.example", "sedona-observation")?;
    let source = PrescribedId::abstract_source("soloc.example", "user-estimated-location")?;
    let options = GpsOptions::new(entity, source);
    let epoch = Epoch::from_gregorian_utc(2025, 1, 1, 12, 0, 0, 0);
    let ellipsoidal_height_m = MSL_HEIGHT_M + geoid_undulation_m;
    let observation = GpsObservation {
        latitude_deg: LATITUDE_DEG,
        longitude_deg: LONGITUDE_DEG,
        ellipsoidal_height_m,
        epoch,
        // Conventional east/north/up basis, not the unknown camera orientation.
        orientation: GpsOrientation::LocalEnu,
    };
    let converted = convert_gps_observation(&observation, &options, &almanac)?;
    let mut builder = EntityBuilder::new(1);
    converted.append_to(&mut builder);
    let mut ledger = Ledger::for_schema::<EntitySchema>()?;
    ledger.append(builder.flush())?;

    // One actual GPS observation plus one display-frame anchor at the same instant.
    // This preserves the standard EntitySchema and does not invent GPX provenance.
    ledger.append(itrf93_frame_snapshot(&almanac, epoch)?)?;
    let itrf93 = PrescribedId::astronomical(399, 3000)?;
    let gcrf = PrescribedId::astronomical(399, 1)?;
    ledger.register_name(entity, "soloc.example", "sedona-observation")?;
    ledger.register_name(source, "soloc.example", "user-estimated-location")?;
    ledger.register_name(itrf93, ASTRO_AUTHORITY, "ITRF93")?;
    ledger.register_name(gcrf, ASTRO_AUTHORITY, "GCRF")?;
    ledger.register_name(
        PrescribedId::abstract_source("anise", "almanac")?,
        "anise",
        "almanac",
    )?;
    if ledger.row_count() != 2 {
        return Err("expected one observation and one Earth-frame anchor".into());
    }
    if let Some(parent) = output.parent().filter(|path| !path.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    ledger.save_ipc(&output)?;

    let (centuries, nanos) = converted.epoch_parts();
    // Keep the u64 nanosecond count as a decimal string in JSON, so JavaScript
    // readers do not silently round it through a 53-bit number.
    let json = format!(
        r#"{{
  "description": "One estimated Sedona position converted by Soloc's GPS adapter",
  "input": {{
    "latitude_deg": {LATITUDE_DEG},
    "longitude_deg": {LONGITUDE_DEG},
    "approximate_msl_height_m": {MSL_HEIGHT_M},
    "caller_supplied_geoid_undulation_m": {geoid_undulation_m},
    "ellipsoidal_height_m": {ellipsoidal_height_m},
    "height_conversion": "h = H + N; approximate MSL elevation treated as orthometric height",
    "vertical_reference_assumption": "Use EGM96 as an approximate MSL reference; original elevation datum is unknown",
    "original_observation_time": null,
    "assumed_demo_epoch_utc": "2025-01-01T12:00:00Z"
  }},
  "entity_id": "{entity}",
  "entity_name": "soloc.example:sedona-observation",
  "spacetimestamp": {{
    "frame_id": "{itrf93}",
    "frame_name": "ITRF93",
    "source_id": "{source}",
    "source_name": "soloc.example:user-estimated-location",
    "units_pos": "m",
    "timescale_id": "TAI",
    "estimate_type": "ESTIMATED",
    "position": {position:?},
    "quaternion_wxyz": {quaternion:?},
    "duration_centuries": {centuries},
    "duration_ns": "{nanos}",
    "epoch_origin": "2000-01-01T12:00:00 TAI",
    "epoch_tai": "2025-01-01T12:00:37 TAI",
    "position_covariance": null,
    "orientation_covariance": null
  }},
  "velocity": null,
  "angular_velocity": null,
  "acceleration": null,
  "mass_kg": null,
  "state_covariance": null,
  "dimensions": null,
  "orientation_policy": "Conventional local east/north/up axes, not measured camera attitude",
  "frame_approximation": "WGS84 ECEF is treated as ITRF93 for ordinary tracking; no realization or station-motion correction",
  "export": {{
    "gps_observation_rows": 1,
    "earth_frame_anchor_rows": 1,
    "total_rows": 2,
    "parent_chain": "sedona-observation -> ITRF93 -> GCRF",
    "journey": false
  }}
}}
"#,
        position = converted.position_m(),
        quaternion = converted.quaternion_wxyz(),
    );
    let json_output = output.with_extension("json");
    std::fs::write(&json_output, &json)?;
    println!("{json}");
    println!(
        "Saved one GPS observation + one Earth-frame anchor to {}",
        output.display()
    );
    println!("Exact readable values: {}", json_output.display());
    println!("Display names are in the .names.arrow sibling.");
    println!("Demo time is assumed; the photo's actual time and camera attitude are unknown.");
    if output == PathBuf::from(DEFAULT_OUTPUT) {
        println!("http://localhost:5173/?src=/data/sedona-single-gps.arrows");
    }
    Ok(())
}
