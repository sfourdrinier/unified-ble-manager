use serde_json::Value;
use tauri::{ipc::Channel, Runtime, State, WebviewWindow};

use crate::{AuthenticatedCaller, IpcEventSink, IpcValue, PluginState};

#[tauri::command]
pub async fn invoke<R: Runtime>(
    window: WebviewWindow<R>,
    state: State<'_, PluginState>,
    request: Value,
    event_channel: Channel<Value>,
) -> Result<Value, String> {
    let caller = AuthenticatedCaller::from_window(&window);
    let dispatcher = state.dispatcher();
    let request = IpcValue::from_wire(request)?;
    let event_sink = IpcEventSink::new(event_channel);
    let response = dispatcher.dispatch(caller, request, event_sink).await;
    Ok(response.into_wire())
}
