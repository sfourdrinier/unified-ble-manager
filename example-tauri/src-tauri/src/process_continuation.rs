use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use tauri::{Manager, State, Url, WebviewWindow};
use tauri_plugin_unified_ble_manager::{native_continuation_envelope, BtleplugDispatcher};

const REQUEST_BYTES: usize = 512 * 1024;
const RESPONSE_BYTES: usize = 8 * 1024 * 1024;
type Reply = Result<Value, Value>;

async fn quit_readiness<S, SF, H, HF>(shutdown: S, handoff: H) -> Result<bool, Value>
where
    S: FnOnce() -> SF,
    SF: std::future::Future<Output = Result<(), Value>>,
    H: FnOnce() -> HF,
    HF: std::future::Future<Output = Reply>,
{
    shutdown().await?;
    // A zero-length queue is still an owned handoff. Only the native engine's
    // authoritative absence permits exit. Never prepare/ACK on the user's behalf.
    Ok(handoff().await?.is_null())
}

fn bounded_envelope(result: Reply) -> String {
    let encoded = native_continuation_envelope(result);
    if encoded.len() > RESPONSE_BYTES {
        native_continuation_envelope(Err(refusal(
            "protocol.malformed",
            "Application continuation response exceeds transport bound",
        )))
    } else {
        encoded
    }
}

fn refusal(code: &str, detail: &str) -> Value {
    json!({"code":code,"domain":"lifecycle","operation":"reference.process-continuation","detail":detail})
}

fn document(mut url: Url) -> Url {
    url.set_query(None);
    url.set_fragment(None);
    url
}

#[derive(Default)]
struct Admission {
    epoch: u64,
    loaded: bool,
    outstanding: usize,
}
struct Gate {
    document: Url,
    admission: Arc<Mutex<Admission>>,
}
struct Ticket {
    epoch: u64,
    admission: Arc<Mutex<Admission>>,
}
impl Drop for Ticket {
    fn drop(&mut self) {
        self.admission
            .lock()
            .expect("admission mutex poisoned")
            .outstanding -= 1;
    }
}
impl Gate {
    fn new(url: Url) -> Self {
        Self {
            document: document(url),
            admission: Arc::new(Mutex::new(Admission::default())),
        }
    }
    fn started(&self) {
        let mut state = self.admission.lock().expect("admission mutex poisoned");
        state.epoch += 1;
        state.loaded = false;
    }
    fn loaded(&self) {
        self.admission
            .lock()
            .expect("admission mutex poisoned")
            .loaded = true;
    }
    fn verify(&self, state: &Admission, label: &str, url: &Url) -> Result<(), Value> {
        if label != "main" || document(url.clone()) != self.document || !state.loaded {
            return Err(refusal(
                "ownership.denied",
                "Only the loaded main driver document may use this command",
            ));
        }
        Ok(())
    }
    fn admit(&self, label: &str, url: &Url) -> Result<Ticket, Value> {
        let mut state = self.admission.lock().expect("admission mutex poisoned");
        self.verify(&state, label, url)?;
        if state.outstanding >= 8 {
            return Err(refusal(
                "lifecycle.invalid-state",
                "Application command limit reached",
            ));
        }
        state.outstanding += 1;
        Ok(Ticket {
            epoch: state.epoch,
            admission: Arc::clone(&self.admission),
        })
    }
    fn check(&self, label: &str, url: &Url, epoch: u64) -> Result<(), Value> {
        let state = self.admission.lock().expect("admission mutex poisoned");
        self.verify(&state, label, url)?;
        if state.epoch != epoch {
            return Err(refusal(
                "ownership.denied",
                "The admitted document has been retired",
            ));
        }
        Ok(())
    }
}

enum Command {
    Execute(String, String),
    Status,
    Prepare(u32, u32),
    Acknowledge(String),
    RecordingStatus(String),
    RecordingPrepare(String, u32, u32),
    RecordingAcknowledge(String, String),
    RecordingStop(String),
    RecordingClear(String),
}
impl Command {
    fn needs_storage(&self) -> bool {
        match self {
            Self::Execute(_, declaration) => serde_json::from_str::<Value>(declaration)
                .is_ok_and(|value| value.get("recording").is_some()),
            Self::RecordingStatus(..)
            | Self::RecordingPrepare(..)
            | Self::RecordingAcknowledge(..)
            | Self::RecordingStop(..)
            | Self::RecordingClear(..) => true,
            Self::Status | Self::Prepare(..) | Self::Acknowledge(..) => false,
        }
    }
}
fn invalid() -> Value {
    refusal(
        "argument.invalid",
        "Invalid application continuation command",
    )
}
fn exact(value: &Value, keys: &[&str]) -> Result<(), Value> {
    let fields = value.as_object().ok_or_else(invalid)?;
    if fields.len() != keys.len() || keys.iter().any(|key| !fields.contains_key(*key)) {
        return Err(invalid());
    }
    Ok(())
}
fn text(value: &Value, max: usize) -> Result<String, Value> {
    value
        .as_str()
        .filter(|text| !text.is_empty() && text.len() <= max)
        .map(str::to_owned)
        .ok_or_else(invalid)
}
fn integer(value: &Value, max: u32) -> Result<u32, Value> {
    value
        .as_u64()
        .filter(|value| *value > 0 && *value <= u64::from(max))
        .map(|value| value as u32)
        .ok_or_else(invalid)
}
fn recording_id(value: &Value) -> Result<String, Value> {
    let id = text(value, 64)?;
    if !id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(invalid());
    }
    Ok(id)
}
fn parse(request: Value) -> Result<Command, Value> {
    if request.to_string().len() > REQUEST_BYTES {
        return Err(invalid());
    }
    exact(&request, &["operation", "args"])?;
    let args = &request["args"];
    Ok(match request["operation"].as_str().ok_or_else(invalid)? {
        "execute" => {
            exact(args, &["peerId", "declarationJson"])?;
            Command::Execute(
                text(&args["peerId"], 4096)?,
                text(&args["declarationJson"], REQUEST_BYTES)?,
            )
        }
        "status" => {
            exact(args, &[])?;
            Command::Status
        }
        "prepare-claim" => {
            exact(args, &["maxItems", "maxBytes"])?;
            Command::Prepare(
                integer(&args["maxItems"], 2048)?,
                integer(&args["maxBytes"], 4 * 1024 * 1024)?,
            )
        }
        "acknowledge-claim" => {
            exact(args, &["token"])?;
            Command::Acknowledge(text(&args["token"], 4096)?)
        }
        "recording-status" => {
            exact(args, &["id"])?;
            Command::RecordingStatus(recording_id(&args["id"])?)
        }
        "recording-prepare" => {
            exact(args, &["id", "maxItems", "maxBytes"])?;
            Command::RecordingPrepare(
                recording_id(&args["id"])?,
                integer(&args["maxItems"], 2048)?,
                integer(&args["maxBytes"], 4 * 1024 * 1024)?,
            )
        }
        "recording-acknowledge" => {
            exact(args, &["id", "token"])?;
            Command::RecordingAcknowledge(recording_id(&args["id"])?, text(&args["token"], 4096)?)
        }
        "recording-stop" => {
            exact(args, &["id"])?;
            Command::RecordingStop(recording_id(&args["id"])?)
        }
        "recording-clear" => {
            exact(args, &["id"])?;
            Command::RecordingClear(recording_id(&args["id"])?)
        }
        _ => return Err(invalid()),
    })
}

pub struct ProcessContinuation {
    gate: Gate,
    dispatcher: BtleplugDispatcher,
    directory: PathBuf,
    configured: tauri::async_runtime::Mutex<bool>,
    quitting: AtomicBool,
    exit_allowed: AtomicBool,
}
fn create_private_directory(directory: &std::path::Path) -> Result<(), Value> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(directory).map_err(|_| {
        refusal(
            "platform.failure",
            "Application recording directory creation failed",
        )
    })
}
impl ProcessContinuation {
    pub fn new(dispatcher: BtleplugDispatcher, directory: PathBuf, document: Url) -> Self {
        Self {
            dispatcher,
            directory,
            gate: Gate::new(document),
            configured: tauri::async_runtime::Mutex::new(false),
            quitting: AtomicBool::new(false),
            exit_allowed: AtomicBool::new(false),
        }
    }
    pub fn page_load(&self, started: bool) {
        if started {
            self.gate.started();
        } else {
            self.gate.loaded();
        }
    }
    pub fn may_exit(&self) -> bool {
        self.exit_allowed.load(Ordering::SeqCst)
    }
    pub fn request_quit(app: tauri::AppHandle) {
        let state = app.state::<Self>();
        if state.quitting.swap(true, Ordering::SeqCst) {
            return;
        }
        tauri::async_runtime::spawn(async move {
            let state = app.state::<Self>();
            let outcome = quit_readiness(|| async {
                let report = state.dispatcher.authority_shutdown().await;
                if report.orphan_failures.is_empty() && report.core.as_ref().is_none_or(|core| core.is_released()) {
                    Ok(())
                } else {
                    // Preserve every named native component and orphan failure;
                    // keep the same owner/window for an explicit retry.
                    Err(json!({"operation":"reference.process-continuation.quit","state":"release-failed",
                        "core":report.core.map(|core| format!("{core:?}")),"orphanFailures":report.orphan_failures}))
                }
            }, || state.dispatcher.continuation_describe_backlog()).await;
            state.quitting.store(false, Ordering::SeqCst);
            match outcome {
                Ok(true) => {
                    state.exit_allowed.store(true, Ordering::SeqCst);
                    app.exit(0);
                }
                Ok(false) => eprintln!(
                    "{}",
                    json!({"event":"process-quit-refused","reason":"native-handoff-remains-owned","action":"Explicitly claim/export the retained handoff, then retry close"})
                ),
                Err(error) => {
                    eprintln!("{}", json!({"event":"process-quit-refused","error":error}))
                }
            }
        });
    }
    async fn configure(&self) -> Reply {
        let mut configured = self.configured.lock().await;
        if !*configured {
            let directory = self.directory.clone();
            tauri::async_runtime::spawn_blocking(move || create_private_directory(&directory))
                .await
                .map_err(|_| {
                    refusal(
                        "platform.failure",
                        "Application recording directory worker failed",
                    )
                })??;
            self.dispatcher
                .continuation_configure_recording_directory(&self.directory)
                .await?;
            *configured = true;
        }
        Ok(Value::Null)
    }
    async fn dispatch(&self, command: Command) -> Reply {
        match command {
            Command::Execute(peer, declaration) => {
                self.dispatcher
                    .continuation_execute(&peer, &declaration)
                    .await
            }
            Command::Status => self.dispatcher.continuation_describe_backlog().await,
            Command::Prepare(items, bytes) => {
                self.dispatcher
                    .continuation_prepare_claim(items, bytes)
                    .await
            }
            Command::Acknowledge(token) => {
                self.dispatcher.continuation_acknowledge_claim(&token).await
            }
            Command::RecordingStatus(id) => {
                self.dispatcher.continuation_recording_status(&id).await
            }
            Command::RecordingPrepare(id, items, bytes) => {
                self.dispatcher
                    .continuation_recording_prepare(&id, items, bytes)
                    .await
            }
            Command::RecordingAcknowledge(id, token) => {
                self.dispatcher
                    .continuation_recording_acknowledge(&id, &token)
                    .await
            }
            Command::RecordingStop(id) => self.dispatcher.continuation_recording_stop(&id).await,
            Command::RecordingClear(id) => self.dispatcher.continuation_recording_clear(&id).await,
        }
    }
}

#[tauri::command]
pub async fn reference_process_continuation(
    window: WebviewWindow,
    state: State<'_, ProcessContinuation>,
    request: Value,
) -> Result<String, String> {
    let result = async {
        let url = window
            .url()
            .map_err(|_| refusal("ownership.denied", "Caller URL unavailable"))?;
        let ticket = state.gate.admit(window.label(), &url)?;
        let command = parse(request)?;
        // Only recording-dependent operations touch storage. This configures a
        // trusted app-private path, never a renderer-provided path or a radio.
        if command.needs_storage() {
            state.configure().await?;
        }
        state.gate.check(
            window.label(),
            &window
                .url()
                .map_err(|_| refusal("ownership.denied", "Caller URL unavailable"))?,
            ticket.epoch,
        )?;
        let result = state.dispatch(command).await;
        state.gate.check(
            window.label(),
            &window
                .url()
                .map_err(|_| refusal("ownership.denied", "Caller URL unavailable"))?,
            ticket.epoch,
        )?;
        result
    }
    .await;
    Ok(bounded_envelope(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn admission_pins_main_document_and_rejects_retired_replies() {
        let gate = Gate::new("http://127.0.0.1:1420/driver.html".parse().unwrap());
        let url = "http://127.0.0.1:1420/driver.html?host=test"
            .parse()
            .unwrap();
        gate.loaded();
        assert!(gate.admit("other", &url).is_err());
        assert!(gate
            .admit("main", &"https://example.com/driver.html".parse().unwrap())
            .is_err());
        let ticket = gate.admit("main", &url).unwrap();
        gate.started();
        assert!(gate.check("main", &url, ticket.epoch).is_err());
        assert!(gate.admit("main", &url).is_err());
        gate.loaded();
        assert!(gate.admit("main", &url).is_ok());
    }

    #[test]
    fn outstanding_work_is_bounded_and_released_on_drop() {
        let url = "tauri://localhost/driver.html".parse().unwrap();
        let gate = Gate::new(url);
        gate.loaded();
        let tickets: Vec<_> = (0..8)
            .map(|_| gate.admit("main", &gate.document).unwrap())
            .collect();
        assert!(gate.admit("main", &gate.document).is_err());
        drop(tickets);
        assert!(gate.admit("main", &gate.document).is_ok());
    }

    #[test]
    fn commands_reject_paths_unknown_fields_and_invalid_bounds() {
        assert!(parse(json!({"operation":"status","args":{}})).is_ok());
        for request in [
            json!({"operation":"recording-status","args":{"id":"x","directory":"/tmp/x"}}),
            json!({"operation":"recording-clear","args":{"id":"../x"}}),
            json!({"operation":"prepare-claim","args":{"maxItems":0,"maxBytes":1}}),
            json!({"operation":"recording-prepare","args":{"id":"x","maxItems":2049,"maxBytes":1}}),
            json!({"operation":"acknowledge-claim","args":{"token":""}}),
            json!({"operation":"other","args":{}}),
            json!({"operation":"status","args":{},"caller":"main"}),
            json!({"operation":"execute","args":{"peerId":"x","declarationJson":"x".repeat(512*1024)}}),
        ] {
            assert!(parse(request).is_err());
        }
    }

    #[test]
    fn nonrecording_control_does_not_require_storage_configuration() {
        for request in [
            json!({"operation":"status","args":{}}),
            json!({"operation":"execute","args":{"peerId":"peer","declarationJson":"{\"onAppearance\":\"native\"}"}}),
        ] {
            assert!(!parse(request).unwrap().needs_storage());
        }
        assert!(parse(json!({"operation":"execute","args":{"peerId":"peer","declarationJson":"{\"recording\":{}}"}})).unwrap().needs_storage());
    }

    #[test]
    fn trusted_directory_is_created_and_failure_is_explicit() {
        let root = std::env::temp_dir().join(format!(
            "ubm-tauri-app-directory-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let directory = root.join("recordings");
        create_private_directory(&directory).unwrap();
        create_private_directory(&directory).unwrap();
        assert!(directory.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        let file = root.join("file");
        std::fs::write(&file, b"owned test").unwrap();
        assert_eq!(
            create_private_directory(&file).unwrap_err()["code"],
            "platform.failure"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn response_bound_refuses_oversize_without_changing_native_errors() {
        let failure = json!({"code":"platform.failure","domain":"gatt","operation":"gatt.read","detail":"native refusal"});
        let response: Value =
            serde_json::from_str(&bounded_envelope(Err(failure.clone()))).unwrap();
        assert_eq!(response["error"], failure);
        let response: Value =
            serde_json::from_str(&bounded_envelope(Ok(json!("x".repeat(RESPONSE_BYTES))))).unwrap();
        assert_eq!(response["ok"], false);
        assert_eq!(response["error"]["code"], "protocol.malformed");
    }

    #[test]
    fn controlled_quit_keeps_failed_cleanup_and_empty_but_owned_handoff() {
        use std::future::ready;
        let error = json!({"code":"platform.failure","detail":"retained release failure"});
        let refused = tauri::async_runtime::block_on(quit_readiness(
            || ready(Err(error.clone())),
            || ready(Ok(Value::Null)),
        ));
        assert_eq!(refused.unwrap_err(), error);
        assert!(!tauri::async_runtime::block_on(quit_readiness(
            || ready(Ok(())),
            || ready(Ok(json!({"queuedData":0,"queuedControls":0}))),
        ))
        .unwrap());
        assert!(tauri::async_runtime::block_on(quit_readiness(
            || ready(Ok(())),
            || ready(Ok(Value::Null)),
        ))
        .unwrap());
        assert!(tauri::async_runtime::block_on(quit_readiness(
            || ready(Ok(())),
            || ready(Err(error)),
        ))
        .is_err());
    }
}
