use std::path::PathBuf;
use std::sync::Arc;

use anise::almanac::metaload::MetaAlmanac;
use anise::almanac::Almanac;
use serde::Deserialize;
use tokio::signal::unix::{signal, SignalKind};
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
    /// Largest gRPC message accepted or sent, in bytes. Default: 64 MiB (tonic's own is 4 MiB).
    #[serde(default = "default_max_message_size")]
    max_message_size: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            max_message_size: default_max_message_size(),
        }
    }
}

fn default_max_message_size() -> usize {
    service::DEFAULT_MAX_MESSAGE_SIZE
}

fn default_bind() -> String {
    "0.0.0.0:50051".to_string()
}

/// Persistence configuration for the ledger.
///
/// When omitted the server runs entirely in memory and all data is lost on shutdown.
///
/// `ledger_url` takes priority over `ledger_path` when both are set.
#[derive(Deserialize)]
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
    /// Path to a zero-row Arrow IPC file that defines the schema for a fresh ledger.
    /// Ignored when an existing ledger is loaded (schema comes from the IPC file).
    /// When absent and no existing ledger is found, defaults to the standard entity schema.
    schema_path: Option<String>,
    /// Name of the entity-identity column. Default: `"entity_id"`.
    #[serde(default = "default_id_column")]
    id_column: String,
    /// Cap on the ledger's resident memory, e.g. `"5GB"`, `"512MiB"` or `"1000000"` (bytes).
    /// When over it, the oldest rows are evicted for good, except each entity's latest row.
    /// Needs a non-empty `id_column`. Absent: every row is kept.
    memory_limit: Option<String>,
}

// Written out rather than derived: a derived Default would leave `id_column` empty whenever
// there is no `[storage]` table, since `default_id_column` only applies when one is parsed.
impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            ledger_url: None,
            ledger_path: None,
            schema_path: None,
            id_column: default_id_column(),
            memory_limit: None,
        }
    }
}

fn default_id_column() -> String {
    "entity_id".to_string()
}

/// The units [`parse_byte_size`] accepts, matched case-insensitively.
const BYTE_UNITS: &[(&str, usize)] = &[
    ("", 1),
    ("B", 1),
    ("KB", 1_000),
    ("MB", 1_000_000),
    ("GB", 1_000_000_000),
    ("TB", 1_000_000_000_000),
    ("KiB", 1 << 10),
    ("MiB", 1 << 20),
    ("GiB", 1 << 30),
    ("TiB", 1 << 40),
];

/// Parses a byte count written as a whole number with an optional unit from
/// [`BYTE_UNITS`]: `"5GB"`, `"512 MiB"`, `"1000000"`.
fn parse_byte_size(text: &str) -> Result<usize, String> {
    let text = text.trim();
    let (digits, unit) = text.split_at(
        text.find(|c: char| !c.is_ascii_digit())
            .unwrap_or(text.len()),
    );
    let scale = BYTE_UNITS
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(unit.trim()))
        .map(|&(_, scale)| scale);
    match (digits.parse::<usize>(), scale) {
        (Ok(count), Some(scale)) => count
            .checked_mul(scale)
            .ok_or_else(|| format!("byte size '{text}' overflows")),
        _ => Err(format!(
            "invalid byte size '{text}': expected a whole number with an optional unit \
             (B, KB, MB, GB, TB, KiB, MiB, GiB, TiB), e.g. \"5GB\" or \"512MiB\""
        )),
    }
}

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

    let ledger_path = cfg.storage.ledger_path.map(PathBuf::from);
    let ledger_url = cfg.storage.ledger_url;
    let schema_path = cfg.storage.schema_path.map(PathBuf::from);
    let id_column = cfg.storage.id_column;
    let memory_limit = cfg
        .storage
        .memory_limit
        .as_deref()
        .map(parse_byte_size)
        .transpose()
        .map_err(|e| format!("storage.memory_limit: {e}"))?;

    let state = Arc::new(
        ServerState::new(
            almanac,
            ledger_path,
            ledger_url,
            schema_path,
            id_column,
            memory_limit,
        )
        .await,
    );
    let service = SolocFlightService::new(state.clone());

    // SIGTERM handler: drain in-flight requests then save the ledger.
    // This is the safety-net path; the nominal path is DoAction("save_ledger").
    let mut sigterm = signal(SignalKind::terminate())?;
    let shutdown = async move {
        sigterm.recv().await;
    };

    eprintln!("soloc-server listening on {addr}");
    Server::builder()
        .add_service(service.into_server(cfg.server.max_message_size))
        .serve_with_shutdown(addr, shutdown)
        .await?;

    eprintln!("soloc-server: shutdown signal received, saving ledger ...");
    state.persist_ledger().await;
    eprintln!("soloc-server: goodbye");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With no config file, or one with no `[storage]` table, the ledger must still be keyed
    /// by `entity_id`; an empty id column collapses `current_state` and entity frames.
    #[test]
    fn test_id_column_defaults_without_a_storage_table() {
        assert_eq!(Config::default().storage.id_column, "entity_id");
        let parsed: Config = toml::from_str("[server]\nbind = \"127.0.0.1:1\"").unwrap();
        assert_eq!(parsed.storage.id_column, "entity_id");
    }

    /// The message size defaults the same way whether `[server]` is absent or present
    /// without the key, and a configured value is read.
    #[test]
    fn test_max_message_size_defaults_and_parses() {
        let default = service::DEFAULT_MAX_MESSAGE_SIZE;
        assert_eq!(Config::default().server.max_message_size, default);
        let parsed: Config = toml::from_str("[server]\nbind = \"127.0.0.1:1\"").unwrap();
        assert_eq!(parsed.server.max_message_size, default);
        let parsed: Config = toml::from_str("[server]\nmax_message_size = 1024").unwrap();
        assert_eq!(parsed.server.max_message_size, 1024);
    }

    #[test]
    fn test_memory_limit_is_absent_by_default() {
        assert_eq!(Config::default().storage.memory_limit, None);
        let parsed: Config = toml::from_str("[storage]\nid_column = \"entity_id\"").unwrap();
        assert_eq!(parsed.storage.memory_limit, None);
        let parsed: Config = toml::from_str("[storage]\nmemory_limit = \"5GB\"").unwrap();
        assert_eq!(parsed.storage.memory_limit.as_deref(), Some("5GB"));
    }

    #[test]
    fn test_parse_byte_size_units() {
        for (text, bytes) in [
            ("1000000", 1_000_000),
            ("5GB", 5_000_000_000),
            ("512MiB", 512 << 20),
            ("512 mib", 512 << 20),
            (" 2 KB ", 2_000),
            ("7B", 7),
            ("1TiB", 1 << 40),
        ] {
            assert_eq!(parse_byte_size(text), Ok(bytes), "{text:?}");
        }
    }

    #[test]
    fn test_parse_byte_size_rejects_bad_input() {
        for text in [
            "",
            "GB",
            "1.5GB",
            "-1",
            "5 XB",
            "5GBB",
            "99999999999999999999",
        ] {
            assert!(
                parse_byte_size(text).is_err(),
                "{text:?} should be rejected"
            );
        }
        assert!(
            parse_byte_size("18446744073709551615GB").is_err(),
            "overflow"
        );
    }
}
