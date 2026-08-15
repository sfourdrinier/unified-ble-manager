use serde_json::Value;
use tauri::{ipc::Channel, Runtime, State, WebviewWindow};

use crate::{AuthenticatedCaller, PluginState};

#[tauri::command]
pub async fn invoke<R: Runtime>(
    window: WebviewWindow<R>,
    state: State<'_, PluginState>,
    request: Value,
    event_channel: Channel<Value>,
) -> Value {
    let caller = AuthenticatedCaller::from_window(&window);
    state.dispatch(caller, request, event_channel).await
}
