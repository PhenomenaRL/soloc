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
}

impl ServerState {
    pub async fn new(
        almanac: Almanac,
        ledger_path: Option<PathBuf>,
        ledger_url: Option<String>,
        schema_path: Option<PathBuf>,
        id_column: String,
    ) -> Self {
        let ledger = if let Some(ref url) = ledger_url {
            match object_store_download(url, &id_column).await {
                Ok(l) => {
                    eprintln!(
                        "soloc-server: ledger loaded from {url} ({} batches)",
                        l.len()
                    );
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
                            "soloc-server: ledger loaded from {:?} ({} batches)",
                            p,
                            l.len()
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

        Self {
            ledger: Arc::new(RwLock::new(ledger)),
            almanac: Arc::new(RwLock::new(almanac)),
            ledger_path,
            ledger_url,
            id_column,
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
                    Ok(bytes) => Some((bytes, ledger.len())),
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
            if let Some((bytes, n_batches)) = serialized {
                match object_store_upload(bytes, url).await {
                    Ok(()) => {
                        eprintln!("soloc-server: ledger saved to {url} ({n_batches} batches)")
                    }
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
                            "soloc-server: ledger saved to {:?} ({} batches)",
                            path,
                            ledger.len()
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
