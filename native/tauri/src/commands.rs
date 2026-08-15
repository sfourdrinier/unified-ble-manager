use serde_json::Value;
use tauri::{ipc::Channel, Runtime, State, WebviewWindow};

use crate::{AuthenticatedCaller, PluginState};

#[tauri::command]
pub async fn invoke<R: Runtime>(
    window: WebviewWindow<R>,
    state: State<'_, PluginState>,
    request: Value,
    event_channel: Channel<Value>,
) -> Result<Value, String> {
    let caller = AuthenticatedCaller::from_window(&window);
    let dispatcher = state.dispatcher();
    Ok(dispatcher.dispatch(caller, request, event_channel).await)
}
