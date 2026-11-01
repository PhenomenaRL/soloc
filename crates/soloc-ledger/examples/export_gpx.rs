//! Export a simulated Halifax journey for Earth-relative, gap-aware playback.
//!
//! Requires an explicitly supplied local Earth BPC covering January 1, 2025:
//! SOLOC_KERNEL_PATHS=/path/earth_latest_high_prec.bpc cargo run -p soloc-ledger \
//!   --features gpx --example export_gpx -- visualizer/public/data/halifax-gpx.arrows
//! Load the output and its `.names.arrow` sibling with the visualizer file picker
//! or drag-and-drop. No kernel downloads or viewer-specific frame relabeling occur.

use std::{error::Error, path::PathBuf};

use anise::prelude::Almanac;
use soloc_ledger::gpx::playback::{GpxPlaybackOptions, import_gpx_for_playback};
use spacetimestamp::gps::gpx::{
    GpxEvent, GpxHeightPolicy, GpxImportEvent, GpxImportOptions, GpxMapper, GpxMapping,
    GpxObjectId, GpxPointErrorPolicy, GpxReader, GpxTimePolicy, WaypointPolicy,
};
use spacetimestamp::gps::{GpsOptions, GpsOrientation};
use spacetimestamp::{EstimateType, PrescribedId};

const JOURNEY: &[u8] = include_bytes!("../tests/fixtures/gpx/halifax.gpx");

fn main() -> Result<(), Box<dyn Error>> {
    let paths = std::env::var_os("SOLOC_KERNEL_PATHS")
        .filter(|paths| !paths.is_empty())
        .ok_or("set SOLOC_KERNEL_PATHS to a local Earth .bpc covering January 1, 2025")?;
    let mut almanac = Almanac::default();
    for path in std::env::split_paths(&paths) {
        if !path.is_file() {
            return Err(format!("kernel is not a local file: {}", path.display()).into());
        }
        almanac = almanac.load(path.to_str().ok_or("kernel path must be UTF-8")?)?;
    }
    let output = std::env::args_os().nth(1).map_or_else(
        || PathBuf::from("visualizer/public/data/halifax-gpx.arrows"),
        PathBuf::from,
    );
    if std::env::args_os().nth(2).is_some() {
        return Err("usage: export_gpx [output.arrows]".into());
    }
    let entity = PrescribedId::new("soloc.example", "halifax-vessel")?;
    let receiver = PrescribedId::abstract_source("soloc.example", "synthetic-gpx")?;
    let mut gps = GpsOptions::new(entity, receiver);
    gps.estimate_type = EstimateType::SIMULATED;
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
    let mut export = import_gpx_for_playback(
        GpxReader::new(JOURNEY, source, WaypointPolicy::Reject),
        &mapper,
        &almanac,
        GpxPlaybackOptions::new(
            GpxImportOptions::new(2, GpxPointErrorPolicy::Strict)?,
            60.0,
            10_000,
        )?,
        |event| match event {
            GpxImportEvent::Input(input) if !matches!(input, GpxEvent::Point(_)) => {
                eprintln!("source: {input:?}");
            }
            GpxImportEvent::Mapped { point, mapping } => {
                eprintln!("mapped: {} / {mapping:?}", point.point.context);
            }
            GpxImportEvent::Skipped(error) => eprintln!("skipped: {error}"),
            GpxImportEvent::Failed(error) => eprintln!("failed: {error}"),
            _ => {}
        },
    )?;
    export.register_name(entity, "soloc.example", "halifax-vessel")?;
    export.register_name(receiver, "soloc.example", "synthetic-gpx")?;
    if let Some(parent) = output.parent().filter(|path| !path.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    export.save_ipc(&output)?;
    println!(
        "Exported {} observations and {} ITRF93 frame samples to {}",
        export.report().submitted,
        export.frame_samples(),
        output.display()
    );
    println!("The .names.arrow sibling contains display names. Both GPX segments remain separate.");
    println!(
        "The marker is absent between 12:00:20 and 12:05:00 UTC; stored and displayed epochs use TAI."
    );
    Ok(())
}
