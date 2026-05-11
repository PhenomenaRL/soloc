use std::path::PathBuf;
use std::sync::Arc;

use anise::almanac::Almanac;
use anise::almanac::metaload::MetaAlmanac;
use arrow_flight::flight_service_server::FlightServiceServer;
use tonic::transport::Server;

mod service;
mod state;

use service::SolocFlightService;
use state::ServerState;

/// Loads the Almanac from the environment.
///
/// Resolution order:
/// 1. `SOLOC_KERNEL_PATHS` — colon-separated list of local BSP/PCK/BPC files.
///    Use this in production (ECS) where kernels are mounted from EFS or bundled
///    in the image. No network access required.
/// 2. `MetaAlmanac::latest()` — downloads DE440s + high-precision Earth/Moon kernels
///    from NAIF and caches them locally. Convenient for local development.
/// 3. Empty `Almanac::default()` with a warning — server starts but all
///    astronomical frame transforms will fail at call time.
fn load_almanac() -> Almanac {
    if let Ok(kernel_paths) = std::env::var("SOLOC_KERNEL_PATHS") {
        let mut almanac = Almanac::default();
        for path in kernel_paths.split(':').filter(|p| !p.is_empty()) {
            // Clone before load: Almanac::load takes ownership and gives no way to
            // recover the original on error, so we keep the pre-load state alive.
            match almanac.clone().load(path) {
                Ok(loaded) => {
                    eprintln!("soloc-server: loaded kernel {path}");
                    almanac = loaded;
                }
                Err(e) => {
                    eprintln!("soloc-server: WARNING — failed to load kernel '{path}': {e}");
                }
            }
        }
        return almanac;
    }

    eprintln!("soloc-server: SOLOC_KERNEL_PATHS not set, attempting MetaAlmanac::latest() ...");
    match MetaAlmanac::latest() {
        Ok(almanac) => {
            eprintln!("soloc-server: MetaAlmanac loaded successfully");
            almanac
        }
        Err(e) => {
            eprintln!(
                "soloc-server: WARNING — MetaAlmanac::latest() failed ({e}). \
                 Astronomical frame transforms will not work. \
                 Set SOLOC_KERNEL_PATHS to one or more BSP/PCK files (colon-separated)."
            );
            Almanac::default()
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = "0.0.0.0:50051".parse()?;
    let almanac = load_almanac();
    // Optional first arg: path to persist the frame registry across restarts.
    let registry_path: Option<PathBuf> = std::env::args().nth(1).map(PathBuf::from);

    let state = Arc::new(ServerState::new(almanac, registry_path));
    let service = SolocFlightService::new(state);

    eprintln!("soloc-server listening on {addr}");
    Server::builder()
        .add_service(FlightServiceServer::new(service))
        .serve(addr)
        .await?;

    Ok(())
}
