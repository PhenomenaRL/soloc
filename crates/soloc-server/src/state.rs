use anise::almanac::Almanac;
use soloc::ledger::Ledger;
use spacetimestamp::schema::FrameRegistry;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

pub struct ServerState {
    pub ledger: Arc<RwLock<Ledger>>,
    pub registry: Arc<RwLock<FrameRegistry>>,
    pub almanac: Arc<RwLock<Almanac>>,
    pub registry_path: Option<PathBuf>,
    pub ledger_path: Option<PathBuf>,
    /// Object-store URL for the ledger (e.g. `s3://bucket/key`, `gs://bucket/key`,
    /// `az://container/key`, `file:///abs/path`).  When set, takes priority over
    /// `ledger_path` for both load-on-startup and save-on-shutdown.
    pub ledger_url: Option<String>,
}

impl ServerState {
    pub async fn new(
        almanac: Almanac,
        registry_path: Option<PathBuf>,
        ledger_path: Option<PathBuf>,
        ledger_url: Option<String>,
    ) -> Self {
        let registry = registry_path
            .as_ref()
            .filter(|p| p.exists())
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|json| FrameRegistry::from_json(&json).ok())
            .unwrap_or_default();

        let ledger = if let Some(ref url) = ledger_url {
            match object_store_download(url).await {
                Ok(l) => {
                    eprintln!("soloc-server: ledger loaded from {url} ({} batches)", l.len());
                    l
                }
                Err(e) => {
                    eprintln!(
                        "soloc-server: WARNING — object-store load failed: {e}. \
                         Starting with empty ledger."
                    );
                    Ledger::new()
                }
            }
        } else {
            ledger_path
                .as_ref()
                .filter(|p| p.exists())
                .and_then(|p| {
                    match Ledger::load_ipc(p) {
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
                })
                .unwrap_or_else(Ledger::new)
        };

        Self {
            ledger: Arc::new(RwLock::new(ledger)),
            registry: Arc::new(RwLock::new(registry)),
            almanac: Arc::new(RwLock::new(almanac)),
            registry_path,
            ledger_path,
            ledger_url,
        }
    }

    pub fn persist_registry(&self, registry: &FrameRegistry) {
        if let Some(ref path) = self.registry_path {
            if let Ok(json) = registry.to_json() {
                let _ = std::fs::write(path, json);
            }
        }
    }

    /// Persists the ledger to the configured object-store URL (if set) or the local path.
    pub async fn persist_ledger(&self) {
        if let Some(ref url) = self.ledger_url {
            match self.ledger.read() {
                Ok(ledger) => {
                    match object_store_upload(&ledger, url).await {
                        Ok(()) => eprintln!(
                            "soloc-server: ledger saved to {url} ({} batches)",
                            ledger.len()
                        ),
                        Err(e) => {
                            eprintln!("soloc-server: WARNING — object-store upload failed: {e}")
                        }
                    }
                }
                Err(_) => {
                    eprintln!("soloc-server: WARNING — ledger lock poisoned, skipping save")
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

/// Downloads and deserialises a ledger from any object-store URL.
///
/// The URL scheme selects the backend: `s3://`, `gs://`, `az://`, `file://`, etc.
/// Credentials are resolved from the standard environment-variable chain for each provider
/// (e.g. `AWS_ACCESS_KEY_ID` / ECS task-role IAM for S3, `GOOGLE_APPLICATION_CREDENTIALS`
/// for GCS, `AZURE_STORAGE_ACCOUNT_KEY` for Azure Blob).
async fn object_store_download(url_str: &str) -> Result<Ledger, String> {
    use object_store::ObjectStore;

    let url =
        url::Url::parse(url_str).map_err(|e| format!("invalid object-store URL: {e}"))?;
    let (store, path) = object_store::parse_url(&url)
        .map_err(|e| format!("object-store URL parse failed: {e}"))?;

    let bytes = store
        .get(&path)
        .await
        .map_err(|e| format!("object-store get '{url_str}' failed: {e}"))?
        .bytes()
        .await
        .map_err(|e| format!("object-store read bytes failed: {e}"))?;

    Ledger::load_ipc_from_bytes(&bytes)
}

/// Serialises and uploads the ledger to any object-store URL.
async fn object_store_upload(ledger: &Ledger, url_str: &str) -> Result<(), String> {
    use object_store::ObjectStore;

    let raw = ledger.save_ipc_to_bytes()?;

    let url =
        url::Url::parse(url_str).map_err(|e| format!("invalid object-store URL: {e}"))?;
    let (store, path) = object_store::parse_url(&url)
        .map_err(|e| format!("object-store URL parse failed: {e}"))?;

    store
        .put(&path, raw.into())
        .await
        .map_err(|e| format!("object-store put '{url_str}' failed: {e}"))?;

    Ok(())
}
