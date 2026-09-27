//! Trusted main-process durable access, including when no BLE authority exists.
use napi::Result;
use napi_derive::napi;
use std::sync::Arc;
use ubm_desktop::continuation::{envelope, recording_failure, NativeContinuation};
use ubm_desktop::continuation_journal::{run_blocking, JournalRegistry};

#[napi]
pub struct ContinuationRecordingStore {
    registry: Arc<JournalRegistry>,
    engine: Option<NativeContinuation>,
}

impl ContinuationRecordingStore {
    pub(crate) fn from_engine(engine: NativeContinuation) -> Self {
        Self {
            registry: engine.recording_registry(),
            engine: Some(engine),
        }
    }
}

#[napi]
impl ContinuationRecordingStore {
    #[napi(constructor, catch_unwind)]
    pub fn new() -> Self {
        Self {
            registry: Arc::default(),
            engine: None,
        }
    }

    /// Host-chosen app-private directory; never forward this authority to IPC.
    #[napi(catch_unwind)]
    pub async fn configure_directory(&self, directory: String) -> Result<String> {
        let registry = self.registry.clone();
        Ok(envelope(
            run_blocking(move || {
                registry
                    .configure_directory(std::path::Path::new(&directory))
                    .map_err(recording_failure)
            })
            .await,
        ))
    }
    #[napi(catch_unwind)]
    pub async fn status(&self, id: String) -> Result<String> {
        let registry = self.registry.clone();
        Ok(envelope(
            run_blocking(move || {
                registry
                    .get(&id)
                    .and_then(|journal| journal.status())
                    .map_err(recording_failure)
            })
            .await,
        ))
    }
    #[napi(catch_unwind)]
    pub async fn prepare(&self, id: String, max_items: u32, max_bytes: u32) -> Result<String> {
        let registry = self.registry.clone();
        Ok(envelope(
            run_blocking(move || {
                registry
                    .get(&id)
                    .and_then(|journal| journal.prepare(max_items, max_bytes))
                    .map_err(recording_failure)
            })
            .await,
        ))
    }
    #[napi(catch_unwind)]
    pub async fn acknowledge(&self, id: String, token: String) -> Result<String> {
        let registry = self.registry.clone();
        Ok(envelope(
            run_blocking(move || {
                registry
                    .get(&id)
                    .and_then(|journal| journal.acknowledge(&token))
                    .map_err(recording_failure)
            })
            .await,
        ))
    }
    #[napi(catch_unwind)]
    pub async fn stop(&self, id: String) -> Result<String> {
        let registry = self.registry.clone();
        let engine = self.engine.clone();
        Ok(envelope(
            run_blocking(move || {
                if let Some(engine) = engine {
                    engine.recording_stop(&id)
                } else {
                    let mut receipt = registry
                        .get(&id)
                        .and_then(|journal| journal.stop())
                        .map_err(recording_failure)?;
                    receipt["radioRelease"] = "not-requested".into();
                    Ok(receipt)
                }
            })
            .await,
        ))
    }
    #[napi(catch_unwind)]
    pub async fn clear(&self, id: String) -> Result<String> {
        let registry = self.registry.clone();
        Ok(envelope(
            run_blocking(move || {
                registry
                    .get(&id)
                    .and_then(|journal| journal.clear())
                    .map_err(recording_failure)
            })
            .await,
        ))
    }
}

impl Default for ContinuationRecordingStore {
    fn default() -> Self {
        Self::new()
    }
}
