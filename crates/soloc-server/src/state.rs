use anise::almanac::Almanac;
use soloc::ledger::Ledger;
use spacetimestamp::schema::FrameRegistry;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

pub struct ServerState {
    pub ledger: Arc<RwLock<Ledger>>,
    pub registry: Arc<RwLock<FrameRegistry>>,
    pub almanac: Almanac,
    pub registry_path: Option<PathBuf>,
}

impl ServerState {
    pub fn new(almanac: Almanac, registry_path: Option<PathBuf>) -> Self {
        let registry = registry_path
            .as_ref()
            .filter(|p| p.exists())
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|json| FrameRegistry::from_json(&json).ok())
            .unwrap_or_default();

        Self {
            ledger: Arc::new(RwLock::new(Ledger::new())),
            registry: Arc::new(RwLock::new(registry)),
            almanac,
            registry_path,
        }
    }

    pub fn persist_registry(&self, registry: &FrameRegistry) {
        if let Some(ref path) = self.registry_path {
            if let Ok(json) = registry.to_json() {
                let _ = std::fs::write(path, json);
            }
        }
    }
}
