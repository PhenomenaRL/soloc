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
}

impl ServerState {
    pub fn new(
        almanac: Almanac,
        registry_path: Option<PathBuf>,
        ledger_path: Option<PathBuf>,
    ) -> Self {
        let registry = registry_path
            .as_ref()
            .filter(|p| p.exists())
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|json| FrameRegistry::from_json(&json).ok())
            .unwrap_or_default();

        let ledger = ledger_path
            .as_ref()
            .filter(|p| p.exists())
            .and_then(|p| {
                match Ledger::load_ipc(p) {
                    Ok(l) => {
                        eprintln!("soloc-server: ledger loaded from {:?} ({} batches)", p, l.len());
                        Some(l)
                    }
                    Err(e) => {
                        eprintln!("soloc-server: WARNING — failed to load ledger from {:?}: {e}", p);
                        None
                    }
                }
            })
            .unwrap_or_else(Ledger::new);

        Self {
            ledger: Arc::new(RwLock::new(ledger)),
            registry: Arc::new(RwLock::new(registry)),
            almanac: Arc::new(RwLock::new(almanac)),
            registry_path,
            ledger_path,
        }
    }

    pub fn persist_registry(&self, registry: &FrameRegistry) {
        if let Some(ref path) = self.registry_path {
            if let Ok(json) = registry.to_json() {
                let _ = std::fs::write(path, json);
            }
        }
    }

    pub fn persist_ledger(&self) {
        if let Some(ref path) = self.ledger_path {
            match self.ledger.read() {
                Ok(ledger) => {
                    if let Err(e) = ledger.save_ipc(path) {
                        eprintln!("soloc-server: WARNING — failed to save ledger to {:?}: {e}", path);
                    } else {
                        eprintln!("soloc-server: ledger saved to {:?} ({} batches)", path, ledger.len());
                    }
                }
                Err(_) => eprintln!("soloc-server: WARNING — ledger lock poisoned, skipping save"),
            }
        }
    }
}
