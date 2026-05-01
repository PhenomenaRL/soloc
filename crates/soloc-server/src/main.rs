use std::path::PathBuf;
use std::sync::Arc;

use anise::almanac::Almanac;
use arrow_flight::flight_service_server::FlightServiceServer;
use tonic::transport::Server;

mod service;
mod state;

use service::SolocFlightService;
use state::ServerState;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = "0.0.0.0:50051".parse()?;
    let almanac = Almanac::default();
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
