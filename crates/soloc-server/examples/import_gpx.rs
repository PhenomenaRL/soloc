//! Import explicitly selected GPX objects through the existing Flight endpoint.
//! See the server README for arguments, assumptions and retention requirements.

use std::{fs::File, io::BufReader};

use anise::prelude::Almanac;
use arrow_flight::flight_service_client::FlightServiceClient;
use soloc_server::gpx::submit_batch;
use spacetimestamp::{
    gps::{gpx::*, GpsOptions, GpsOrientation},
    PrescribedId,
};

const USAGE: &str = "usage: import_gpx <http://host:port> <file.gpx> <authority> \
    <entity-name> <receiver-name> <max-rows> <max-message-bytes> \
    --ellipsoidal-elevation <track:N|route:N>...";

fn object_id(text: &str) -> Result<GpxObjectId, String> {
    let (kind, ordinal) = text
        .split_once(':')
        .ok_or_else(|| format!("invalid object {text:?}; expected track:N or route:N"))?;
    let ordinal = ordinal
        .parse::<usize>()
        .map_err(|error| format!("invalid object {text:?}: {error}"))?;
    match kind {
        "track" => Ok(GpxObjectId::Track(ordinal)),
        "route" => Ok(GpxObjectId::Route(ordinal)),
        _ => Err(format!(
            "invalid object {text:?}; expected track:N or route:N"
        )),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    if arguments.len() < 9 || arguments[7] != "--ellipsoidal-elevation" {
        return Err(USAGE.into());
    }
    let endpoint = &arguments[0];
    let source = &arguments[1];
    let options = GpxImportOptions::new(arguments[5].parse()?, GpxPointErrorPolicy::Strict)?;
    let max_message_size: usize = arguments[6].parse()?;
    if max_message_size == 0 {
        return Err("max-message-bytes must be greater than zero".into());
    }
    let mapping = GpxMapping {
        gps: GpsOptions::new(
            PrescribedId::new(&arguments[2], &arguments[3])?,
            PrescribedId::abstract_source(&arguments[2], &arguments[4])?,
        ),
        // The required flag is the caller's explicit vertical-datum declaration.
        height: GpxHeightPolicy::EllipsoidalElevation,
        time: GpxTimePolicy::RequireTimestamp,
        orientation: GpsOrientation::LocalEnu,
    };
    let mappings = arguments[8..]
        .iter()
        .map(|text| object_id(text).map(|object| (object, mapping)))
        .collect::<Result<Vec<_>, _>>()?;
    let mapper = GpxMapper::new(source, mappings)?;
    let reader = GpxReader::new(
        BufReader::new(File::open(source)?),
        source,
        WaypointPolicy::Reject,
    );
    let channel = tonic::transport::Channel::from_shared(endpoint.clone())?
        .connect()
        .await?;
    let client = FlightServiceClient::new(channel);
    let report = import_gpx_async(
        reader,
        &mapper,
        &Almanac::default(),
        options,
        |batch| submit_batch(client.clone(), batch, max_message_size),
        |event| {
            // Stream provenance and segment gaps alongside the rows; redirect
            // stderr to a file to keep it without buffering the journey in RAM.
            match event {
                GpxImportEvent::Input(input) => eprintln!("input: {input:?}"),
                GpxImportEvent::Mapped { point, mapping } => eprintln!(
                    "mapped: {} height={:?} time={:?} mapping={mapping:?}",
                    point.point.context, point.height_policy, point.time_origin
                ),
                GpxImportEvent::Skipped(error) => eprintln!("skipped: {error}"),
                GpxImportEvent::Submitted(batch) => eprintln!("submitted: {batch:?}"),
                GpxImportEvent::Failed(failure) => eprintln!("failed: {failure:?}"),
            }
        },
    )
    .await;
    println!("{report:#?}");
    if !report.complete {
        return Err("import incomplete; inspect confirmed, uncertain and unsubmitted counts before any manual retry".into());
    }
    Ok(())
}
