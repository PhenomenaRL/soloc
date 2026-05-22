use std::path::PathBuf;
use std::sync::Arc;

use anise::almanac::Almanac;
use anise::almanac::metaload::MetaAlmanac;
use arrow_flight::flight_service_server::FlightServiceServer;
use serde::Deserialize;
use tokio::signal::unix::{SignalKind, signal};
use tonic::transport::Server;

mod service;
mod state;

use service::SolocFlightService;
use state::ServerState;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Server configuration loaded from `config.toml` (or `$SOLOC_CONFIG`).
///
/// All fields are optional and fall back to sensible defaults so the server
/// can start with zero configuration for local development.
#[derive(Deserialize, Default)]
struct Config {
    #[serde(default)]
    server: ServerConfig,
    #[serde(default)]
    storage: StorageConfig,
    #[serde(default)]
    ephemeris: EphemerisConfig,
}

#[derive(Deserialize)]
struct ServerConfig {
    /// TCP address to bind. Default: `0.0.0.0:50051`.
    #[serde(default = "default_bind")]
    bind: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self { bind: default_bind() }
    }
}

fn default_bind() -> String {
    "0.0.0.0:50051".to_string()
}

/// Persistence configuration for the ledger and frame registry.
///
/// When omitted the server runs entirely in memory and all data is lost on shutdown.
///
/// `ledger_url` takes priority over `ledger_path` when both are set.
#[derive(Deserialize, Default)]
struct StorageConfig {
    /// Object-store URL for the ledger, e.g.:
    ///   `s3://my-bucket/soloc/ledger.arrows`
    ///   `gs://my-bucket/soloc/ledger.arrows`
    ///   `az://my-container/soloc/ledger.arrows`
    ///   `file:///var/data/ledger.arrows`
    /// Loaded on startup; saved on clean shutdown or `save_ledger` action.
    /// Credentials are resolved from the standard environment-variable chain.
    ledger_url: Option<String>,
    /// Local filesystem path for the ledger (`*.arrows`).
    /// Used when `ledger_url` is not set.
    ledger_path: Option<String>,
    /// Path to a JSON file for the frame registry.
    /// Loaded on startup if the file exists; saved whenever the registry changes.
    registry_path: Option<String>,
    /// Path to a zero-row Arrow IPC file that defines the schema for a fresh ledger.
    /// Ignored when an existing ledger is loaded (schema comes from the IPC file).
    /// When absent and no existing ledger is found, defaults to the standard entity schema.
    schema_path: Option<String>,
    /// Name of the spacetimestamp struct column. Default: `"spacetimestamp"`.
    #[serde(default = "default_sts_column")]
    sts_column: String,
    /// Name of the entity-identity column. Default: `"entity_id"`.
    #[serde(default = "default_id_column")]
    id_column: String,
}

fn default_sts_column() -> String { "spacetimestamp".to_string() }
fn default_id_column()  -> String { "entity_id".to_string() }

/// Ephemeris kernel configuration.
///
/// Resolution order for kernels:
/// 1. `kernels` list in `config.toml`
/// 2. `SOLOC_KERNEL_PATHS` environment variable (colon-separated paths)
/// 3. `MetaAlmanac::latest()` — downloads DE440s + PCK files on first run (~150 MB cached)
/// 4. Empty almanac with a warning — server starts but astronomical transforms will fail
#[derive(Deserialize, Default)]
struct EphemerisConfig {
    /// List of local BSP/PCK/BPC kernel file paths.
    /// Use in production (ECS/EFS) where kernels are pre-mounted.
    #[serde(default)]
    kernels: Vec<String>,
}

/// Loads config from (in order): `$SOLOC_CONFIG`, `./config.toml`, or returns defaults.
fn load_config() -> Config {
    let path = std::env::var("SOLOC_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("config.toml"));

    if path.exists() {
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str::<Config>(&text) {
                Ok(cfg) => {
                    eprintln!("soloc-server: loaded config from {:?}", path);
                    return cfg;
                }
                Err(e) => eprintln!("soloc-server: WARNING — failed to parse {:?}: {e}", path),
            },
            Err(e) => eprintln!("soloc-server: WARNING — failed to read {:?}: {e}", path),
        }
    }

    Config::default()
}

// ---------------------------------------------------------------------------
// Almanac loading
// ---------------------------------------------------------------------------

fn load_almanac(cfg: &EphemerisConfig) -> Almanac {
    // 1. Explicit kernel list from config.
    if !cfg.kernels.is_empty() {
        let mut almanac = Almanac::default();
        for path in &cfg.kernels {
            match almanac.clone().load(path) {
                Ok(loaded) => {
                    eprintln!("soloc-server: loaded kernel {path}");
                    almanac = loaded;
                }
                Err(e) => eprintln!("soloc-server: WARNING — failed to load kernel '{path}': {e}"),
            }
        }
        return almanac;
    }

    // 2. Fall back to SOLOC_KERNEL_PATHS env var.
    if let Ok(kernel_paths) = std::env::var("SOLOC_KERNEL_PATHS") {
        let mut almanac = Almanac::default();
        for path in kernel_paths.split(':').filter(|p| !p.is_empty()) {
            match almanac.clone().load(path) {
                Ok(loaded) => {
                    eprintln!("soloc-server: loaded kernel {path}");
                    almanac = loaded;
                }
                Err(e) => eprintln!("soloc-server: WARNING — failed to load kernel '{path}': {e}"),
            }
        }
        return almanac;
    }

    // 3. MetaAlmanac auto-download.
    eprintln!("soloc-server: no kernels configured, attempting MetaAlmanac::latest() ...");
    match MetaAlmanac::latest() {
        Ok(almanac) => {
            eprintln!("soloc-server: MetaAlmanac loaded successfully");
            almanac
        }
        Err(e) => {
            eprintln!(
                "soloc-server: WARNING — MetaAlmanac::latest() failed ({e}). \
                 Astronomical frame transforms will not work. \
                 Set 'ephemeris.kernels' in config.toml or SOLOC_KERNEL_PATHS."
            );
            Almanac::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = load_config();

    let addr = cfg.server.bind.parse()?;
    let almanac = load_almanac(&cfg.ephemeris);

    let registry_path = cfg.storage.registry_path.map(PathBuf::from);
    let ledger_path   = cfg.storage.ledger_path.map(PathBuf::from);
    let ledger_url    = cfg.storage.ledger_url;
    let schema_path   = cfg.storage.schema_path.map(PathBuf::from);
    let sts_column    = cfg.storage.sts_column;
    let id_column     = cfg.storage.id_column;

    let state = Arc::new(
        ServerState::new(almanac, registry_path, ledger_path, ledger_url, schema_path, sts_column, id_column).await,
    );
    let service = SolocFlightService::new(state.clone());

    // SIGTERM handler: drain in-flight requests then save the ledger.
    // This is the safety-net path; the nominal path is DoAction("save_ledger").
    let mut sigterm = signal(SignalKind::terminate())?;
    let shutdown = async move { sigterm.recv().await; };

    eprintln!("soloc-server listening on {addr}");
    Server::builder()
        .add_service(FlightServiceServer::new(service))
        .serve_with_shutdown(addr, shutdown)
        .await?;

    eprintln!("soloc-server: shutdown signal received, saving ledger ...");
    state.persist_ledger().await;
    eprintln!("soloc-server: goodbye");

    Ok(())
}
