//! Export a simulated Ottawa (YOW) to London Heathrow (LHR) GPS journey.
//!
//! SOLOC_KERNEL_PATHS=/path/earth_latest_high_prec.bpc cargo run --locked \
//!   -p soloc-ledger --features gpx --example export_flight
//!
//! Requires a local Earth BPC covering January 1, 2025, 12:00–19:00 UTC.
//! Writes the playback Arrow ledger, its names sidecar, and the generated GPX.
//! An optional argument replaces the default output path. No kernels are downloaded.

use std::{error::Error, fmt::Write, path::PathBuf};

use anise::prelude::{Almanac, Epoch};
use hifitime::{Duration, TimeScale};
use soloc_ledger::gpx::playback::{GpxPlaybackOptions, import_gpx_for_playback};
use spacetimestamp::gps::gpx::{
    GpxHeightPolicy, GpxImportEvent, GpxImportOptions, GpxMapper, GpxMapping, GpxObjectId,
    GpxPointErrorPolicy, GpxReader, GpxTimePolicy, WaypointPolicy,
};
use spacetimestamp::gps::{GpsOptions, GpsOrientation};
use spacetimestamp::{EstimateType, PrescribedId};

// Approximate airport endpoints, not runway thresholds (sources: visualizer README).
// The path and heights are invented for playback; this is not an airline route,
// a flight plan, a runway trajectory, or a measured aircraft recording.
const OTTAWA_YOW: (f64, f64) = (45.32427, -75.6627);
const LONDON_LHR: (f64, f64) = (51.477581, -0.46137241);
const DURATION_MINUTES: usize = 7 * 60;
const SAMPLE_SECONDS: f64 = 60.0;
const SAMPLE_COUNT: usize = DURATION_MINUTES + 1;
const CLIMB_MINUTES: f64 = 30.0;
const DEPARTURE_HEIGHT_M: f64 = 150.0;
const ARRIVAL_HEIGHT_M: f64 = 100.0;
const CRUISE_HEIGHT_M: f64 = 11_000.0;
const DEFAULT_OUTPUT: &str = "visualizer/public/data/ottawa-london-flight.arrows";
const SOURCE: &str = "ottawa-london-flight.gpx";

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args_os().skip(1);
    let output = args
        .next()
        .map_or_else(|| PathBuf::from(DEFAULT_OUTPUT), PathBuf::from);
    if args.next().is_some()
        || !matches!(
            output.extension().and_then(|s| s.to_str()),
            Some("arrow" | "arrows")
        )
    {
        return Err(
            "usage: export_flight [output.arrows] (use an .arrow or .arrows extension)".into(),
        );
    }
    let paths = std::env::var_os("SOLOC_KERNEL_PATHS")
        .filter(|paths| !paths.is_empty())
        .ok_or("set SOLOC_KERNEL_PATHS to a local Earth .bpc covering January 1, 2025, 12:00–19:00 UTC")?;
    let mut almanac = Almanac::default();
    for path in std::env::split_paths(&paths) {
        if !path.is_file() {
            return Err(format!("kernel is not a local file: {}", path.display()).into());
        }
        almanac = almanac.load(path.to_str().ok_or("kernel path must be UTF-8")?)?;
    }

    let departure = Epoch::from_gregorian_utc(2025, 1, 1, 12, 0, 0, 0);
    let arrival = departure + Duration::from_seconds(DURATION_MINUTES as f64 * SAMPLE_SECONDS);
    let journey = generate_gpx(departure);
    let entity = PrescribedId::new("soloc.example", "ottawa-london-flight")?;
    let receiver = PrescribedId::abstract_source("soloc.example", "synthetic-flight-gpx")?;
    let mut gps = GpsOptions::new(entity, receiver);
    gps.estimate_type = EstimateType::SIMULATED;
    let mapper = GpxMapper::new(
        SOURCE,
        [(
            GpxObjectId::Track(1),
            GpxMapping {
                gps,
                // <ele> values below are explicitly synthetic ellipsoidal heights,
                // not mean-sea-level airport elevations or pressure altitudes.
                height: GpxHeightPolicy::EllipsoidalElevation,
                time: GpxTimePolicy::RequireTimestamp,
                // A local coordinate basis, not the aircraft's heading or attitude.
                // GPX supplies no measured velocities, so those columns stay null.
                orientation: GpsOrientation::LocalEnu,
            },
        )],
    )?;
    let mut export = import_gpx_for_playback(
        GpxReader::new(journey.as_bytes(), SOURCE, WaypointPolicy::Reject),
        &mapper,
        &almanac,
        GpxPlaybackOptions::new(
            GpxImportOptions::new(128, GpxPointErrorPolicy::Strict)?,
            SAMPLE_SECONDS,
            1_000,
        )?,
        |event| match event {
            GpxImportEvent::Submitted(batch) => {
                println!("Imported {} GPS observations", batch.rows)
            }
            GpxImportEvent::Failed(error) => eprintln!("failed: {error}"),
            _ => {}
        },
    )?;
    if export.report().submitted != SAMPLE_COUNT
        || export.frame_samples() != SAMPLE_COUNT
        || export.ledger().row_count() != 2 * SAMPLE_COUNT
    {
        return Err("the complete flight and its ITRF93 frame timeline were not retained".into());
    }
    export.register_name(entity, "soloc.example", "ottawa-london-flight")?;
    export.register_name(receiver, "soloc.example", "synthetic-flight-gpx")?;
    if let Some(parent) = output.parent().filter(|path| !path.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    export.save_ipc(&output)?;
    let gpx_output = output.with_extension("gpx");
    std::fs::write(&gpx_output, journey)?;
    println!(
        "Exported {} SIMULATED observations + {} ITRF93 frame samples = {} rows to {}",
        export.report().submitted,
        export.frame_samples(),
        export.ledger().row_count(),
        output.display()
    );
    println!(
        "Source GPX: {} (one continuous track segment)",
        gpx_output.display()
    );
    println!("Display names are in the .names.arrow sibling.");
    println!(
        "UTC journey: {} → {}",
        departure.to_gregorian_str(TimeScale::UTC),
        arrival.to_gregorian_str(TimeScale::UTC)
    );
    println!(
        "Viewer clock: {} → {}",
        departure.to_gregorian_str(TimeScale::TAI),
        arrival.to_gregorian_str(TimeScale::TAI)
    );
    println!(
        "Synthetic great-circle route; 30-minute climb/descent; 11,000 m ellipsoidal cruise height."
    );
    println!(
        "Local ENU orientation is a coordinate basis, not aircraft attitude; velocity is unknown."
    );
    if output == std::path::Path::new(DEFAULT_OUTPUT) {
        println!("Start the viewer: npm --prefix visualizer run dev");
        println!("http://localhost:5173/?src=/data/ottawa-london-flight.arrows");
        println!("Select ottawa-london-flight, then use the timeline to play or scrub.");
    } else {
        println!(
            "Open the Arrow output and its .names.arrow sibling with the visualizer file picker."
        );
    }
    Ok(())
}

fn generate_gpx(departure: Epoch) -> String {
    let mut gpx = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <gpx version=\"1.1\" creator=\"soloc export_flight\" xmlns=\"http://www.topografix.com/GPX/1/1\">\n\
         <metadata><desc>SIMULATED YOW to LHR flight. Spherical great-circle route; synthetic ellipsoidal elevations in metres; no measured aircraft attitude or velocity.</desc></metadata>\n\
         <trk><name>ottawa-london-flight</name><trkseg>\n",
    );
    for minute in 0..=DURATION_MINUTES {
        let (latitude, longitude) = route_position(minute as f64 / DURATION_MINUTES as f64);
        let height = height_at_minute(minute as f64);
        let epoch = departure + Duration::from_seconds(minute as f64 * SAMPLE_SECONDS);
        let (year, month, day, hour, min, second, _) = epoch.to_gregorian_utc();
        writeln!(
            gpx,
            "<trkpt lat=\"{latitude:.8}\" lon=\"{longitude:.8}\"><ele>{height:.3}</ele><time>{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{second:02}Z</time></trkpt>"
        ).expect("writing to a String cannot fail");
    }
    gpx.push_str("</trkseg></trk></gpx>\n");
    gpx
}

/// Interpolate geodetic latitude/longitude on a sphere, then let the real GPS
/// adapter perform WGS84 conversion. This is an approximate route, not an
/// ellipsoidal geodesic or a simulation of air traffic and wind.
fn route_position(fraction: f64) -> (f64, f64) {
    fn unit_vector((lat, lon): (f64, f64)) -> [f64; 3] {
        let (lat, lon) = (lat.to_radians(), lon.to_radians());
        [lat.cos() * lon.cos(), lat.cos() * lon.sin(), lat.sin()]
    }
    let start = unit_vector(OTTAWA_YOW);
    let end = unit_vector(LONDON_LHR);
    let angle = start
        .iter()
        .zip(end)
        .map(|(a, b)| a * b)
        .sum::<f64>()
        .clamp(-1.0, 1.0)
        .acos();
    let start_weight = ((1.0 - fraction) * angle).sin() / angle.sin();
    let end_weight = (fraction * angle).sin() / angle.sin();
    let point: [f64; 3] = std::array::from_fn(|i| start_weight * start[i] + end_weight * end[i]);
    (
        point[2].atan2(point[0].hypot(point[1])).to_degrees(),
        point[1].atan2(point[0]).to_degrees(),
    )
}

fn height_at_minute(minute: f64) -> f64 {
    // Smoothstep gives zero vertical slope where climb/descent meets cruise.
    let smoothstep = |t: f64| t * t * (3.0 - 2.0 * t);
    if minute < CLIMB_MINUTES {
        DEPARTURE_HEIGHT_M
            + (CRUISE_HEIGHT_M - DEPARTURE_HEIGHT_M) * smoothstep(minute / CLIMB_MINUTES)
    } else if minute > DURATION_MINUTES as f64 - CLIMB_MINUTES {
        ARRIVAL_HEIGHT_M
            + (CRUISE_HEIGHT_M - ARRIVAL_HEIGHT_M)
                * smoothstep((DURATION_MINUTES as f64 - minute) / CLIMB_MINUTES)
    } else {
        CRUISE_HEIGHT_M
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_reaches_both_airports_and_follows_the_short_great_circle() {
        for (fraction, expected) in [(0.0, OTTAWA_YOW), (1.0, LONDON_LHR)] {
            let actual = route_position(fraction);
            assert!((actual.0 - expected.0).abs() < 1e-10);
            assert!((actual.1 - expected.1).abs() < 1e-10);
        }
        // Check arc lengths independently with the haversine formula. Linear
        // latitude/longitude interpolation would not bisect this great circle.
        let arc = |a: (f64, f64), b: (f64, f64)| {
            let h = ((b.0 - a.0).to_radians() / 2.0).sin().powi(2)
                + a.0.to_radians().cos()
                    * b.0.to_radians().cos()
                    * ((b.1 - a.1).to_radians() / 2.0).sin().powi(2);
            2.0 * h.sqrt().asin()
        };
        let half_arc = arc(OTTAWA_YOW, LONDON_LHR) / 2.0;
        let midpoint = route_position(0.5);
        assert!((arc(OTTAWA_YOW, midpoint) - half_arc).abs() < 1e-12);
        assert!((arc(midpoint, LONDON_LHR) - half_arc).abs() < 1e-12);
    }

    #[test]
    fn height_profile_climbs_cruises_and_descends_without_overshoot() {
        assert_eq!(height_at_minute(0.0), DEPARTURE_HEIGHT_M);
        assert_eq!(height_at_minute(DURATION_MINUTES as f64), ARRIVAL_HEIGHT_M);
        let heights: Vec<_> = (0..=DURATION_MINUTES)
            .map(|m| height_at_minute(m as f64))
            .collect();
        assert!(heights[..=30].windows(2).all(|pair| pair[0] < pair[1]));
        assert!(
            heights[30..=390]
                .iter()
                .all(|&height| height == CRUISE_HEIGHT_M)
        );
        assert!(heights[390..].windows(2).all(|pair| pair[0] > pair[1]));
        assert!(
            heights
                .iter()
                .all(|&height| (ARRIVAL_HEIGHT_M..=CRUISE_HEIGHT_M).contains(&height))
        );
    }
}
