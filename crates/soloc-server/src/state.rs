use anise::almanac::Almanac;
use soloc_ledger::ledger::Ledger;
use soloc_ledger::schemas::entity::entity_schema;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

pub struct ServerState {
    pub ledger: Arc<RwLock<Ledger>>,
    pub almanac: Arc<RwLock<Almanac>>,
    pub ledger_path: Option<PathBuf>,
    /// Object-store URL for the ledger (e.g. `s3://bucket/key`, `gs://bucket/key`,
    /// `az://container/key`, `file:///abs/path`).  When set, takes priority over
    /// `ledger_path` for both load-on-startup and save-on-shutdown.
    pub ledger_url: Option<String>,
    pub id_column: String,
    /// `storage.memory_limit` in bytes, applied to every ledger the server holds, including
    /// one replaced by the `load_ledger` action.
    pub memory_limit: Option<usize>,
}

impl ServerState {
    pub async fn new(
        almanac: Almanac,
        ledger_path: Option<PathBuf>,
        ledger_url: Option<String>,
        schema_path: Option<PathBuf>,
        id_column: String,
        memory_limit: Option<usize>,
    ) -> Self {
        let mut ledger = if let Some(ref url) = ledger_url {
            match object_store_download(url, &id_column).await {
                Ok(l) => {
                    eprintln!("soloc-server: ledger loaded from {url} ({})", describe(&l));
                    l
                }
                Err(e) => {
                    eprintln!(
                        "soloc-server: WARNING — object-store load failed: {e}. \
                         Starting with empty ledger."
                    );
                    new_empty_ledger(&schema_path, &id_column)
                }
            }
        } else {
            let loaded = ledger_path.as_ref().filter(|p| p.exists()).and_then(|p| {
                match Ledger::load_ipc(p, &id_column) {
                    Ok(l) => {
                        eprintln!(
                            "soloc-server: ledger loaded from {:?} ({})",
                            p,
                            describe(&l)
                        );
                        Some(l)
                    }
                    Err(e) => {
                        eprintln!(
                            "soloc-server: WARNING — failed to load ledger from {:?}: {e}",
                            p
                        );
                        None
                    }
                }
            });
            loaded.unwrap_or_else(|| new_empty_ledger(&schema_path, &id_column))
        };

        // Stores the limit that took effect, so `load_ledger` never re-applies a refused one.
        let memory_limit = match ledger.set_memory_limit(memory_limit) {
            Ok(()) => {
                if let Some(limit) = memory_limit {
                    eprintln!(
                        "soloc-server: memory limit {limit} bytes ({})",
                        describe(&ledger)
                    );
                }
                memory_limit
            }
            Err(e) => {
                eprintln!("soloc-server: WARNING — memory_limit ignored: {e}. Keeping every row.");
                None
            }
        };

        Self {
            ledger: Arc::new(RwLock::new(ledger)),
            almanac: Arc::new(RwLock::new(almanac)),
            ledger_path,
            ledger_url,
            id_column,
            memory_limit,
        }
    }

    /// Persists the ledger to the configured object-store URL (if set) or the local path.
    pub async fn persist_ledger(&self) {
        if let Some(ref url) = self.ledger_url {
            // Serialize while holding the lock, then release it BEFORE awaiting the
            // network upload. Holding a std lock across an await blocks executor
            // threads and can deadlock (clippy: await_holding_lock).
            let serialized = match self.ledger.read() {
                Ok(ledger) => match ledger.save_ipc_to_bytes() {
                    Ok(bytes) => Some((bytes, describe(&ledger))),
                    Err(e) => {
                        eprintln!("soloc-server: WARNING — ledger serialization failed: {e}");
                        None
                    }
                },
                Err(_) => {
                    eprintln!("soloc-server: WARNING — ledger lock poisoned, skipping save");
                    None
                }
            };
            if let Some((bytes, size)) = serialized {
                match object_store_upload(bytes, url).await {
                    Ok(()) => eprintln!("soloc-server: ledger saved to {url} ({size})"),
                    Err(e) => {
                        eprintln!("soloc-server: WARNING — object-store upload failed: {e}")
                    }
                }
            }
            return;
        }

        if let Some(ref path) = self.ledger_path {
            match self.ledger.read() {
                Ok(ledger) => {
                    if let Err(e) = ledger.save_ipc(path) {
                        eprintln!(
                            "soloc-server: WARNING — failed to save ledger to {:?}: {e}",
                            path
                        );
                    } else {
                        eprintln!(
                            "soloc-server: ledger saved to {:?} ({})",
                            path,
                            describe(&ledger)
                        );
                    }
                }
                Err(_) => {
                    eprintln!("soloc-server: WARNING — ledger lock poisoned, skipping save")
                }
            }
        }
    }
}

/// A ledger's size as the server reports it in log lines and action replies.
pub fn describe(ledger: &Ledger) -> String {
    format!(
        "{} batches, {:.1} MB resident",
        ledger.len(),
        ledger.resident_bytes() as f64 / 1e6
    )
}

/// Creates an empty ledger using `schema_path` (if provided) or the default entity schema.
fn new_empty_ledger(schema_path: &Option<PathBuf>, id_column: &str) -> Ledger {
    if let Some(ref path) = schema_path {
        match spacetimestamp::ipc::read_file_schema(path) {
            Ok(schema) => match Ledger::new(&schema, id_column) {
                Ok(l) => {
                    eprintln!("soloc-server: empty ledger created from schema {:?}", path);
                    return l;
                }
                Err(e) => eprintln!(
                    "soloc-server: WARNING — schema_path schema is invalid: {e}. \
                         Falling back to entity schema."
                ),
            },
            Err(e) => eprintln!(
                "soloc-server: WARNING — failed to read schema_path {:?}: {e}. \
                 Falling back to entity schema.",
                path
            ),
        }
    }

    Ledger::new(&entity_schema(), id_column)
        .expect("entity_schema is always valid for 'spacetimestamp'/'entity_id'")
}

/// Downloads and deserialises a ledger from any object-store URL.
async fn object_store_download(url_str: &str, id_column: &str) -> Result<Ledger, String> {
    use object_store::ObjectStoreExt;

    let url = url::Url::parse(url_str).map_err(|e| format!("invalid object-store URL: {e}"))?;
    let (store, path) =
        object_store::parse_url(&url).map_err(|e| format!("object-store URL parse failed: {e}"))?;

    let bytes = store
        .get(&path)
        .await
        .map_err(|e| format!("object-store get '{url_str}' failed: {e}"))?
        .bytes()
        .await
        .map_err(|e| format!("object-store read bytes failed: {e}"))?;

    Ledger::load_ipc_from_bytes(&bytes, id_column)
}

/// Uploads pre-serialised ledger bytes to any object-store URL.
async fn object_store_upload(raw: Vec<u8>, url_str: &str) -> Result<(), String> {
    use object_store::ObjectStoreExt;

    let url = url::Url::parse(url_str).map_err(|e| format!("invalid object-store URL: {e}"))?;
    let (store, path) =
        object_store::parse_url(&url).map_err(|e| format!("object-store URL parse failed: {e}"))?;

    store
        .put(&path, raw.into())
        .await
        .map_err(|e| format!("object-store put '{url_str}' failed: {e}"))?;

    Ok(())
}
