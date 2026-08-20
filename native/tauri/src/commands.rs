use serde_json::Value;
use tauri::{ipc::JavaScriptChannelId, Runtime, State, Webview, WebviewWindow};

use crate::{AuthenticatedCaller, IpcEventSink, IpcValue, PluginState, ATTACH_REQUEST_KIND};

#[tauri::command]
pub async fn invoke<R: Runtime>(
    window: WebviewWindow<R>,
    state: State<'_, PluginState>,
    request: Value,
    // Optional by contract: only the attach request carries the channel.
    //
    // Taken as a `JavaScriptChannelId` rather than a `Channel` so that this
    // plugin, not the command macro, decides when a Rust `Channel` is built.
    // Every `Channel` Tauri materializes is a distinct object bound to the same
    // JavaScript callback id; dropping any one of them evals `{ end: true }`
    // for that shared id, which unregisters the callback and permanently kills
    // the event stream. Their independent index counters would also desync the
    // receiver. Exactly one `Channel` may exist per attachment.
    event_channel: Option<JavaScriptChannelId>,
) -> Result<Value, String> {
    let caller = AuthenticatedCaller::from_window(&window);
    let dispatcher = state.dispatcher();
    let request = IpcValue::from_wire(request)?;
    let webview: &Webview<R> = window.as_ref();
    // A Channel is materialized only for the attach request, and only here.
    // Clients that supply one on any other request are ignored rather than
    // trusted: merely building a second Channel for this callback id would end
    // the shared JS callback the moment it dropped, so honouring such a request
    // would let an out-of-date client silently destroy its own event stream.
    let event_sink = event_channel
        .filter(|_| is_attach_request(&request))
        .map(|id| IpcEventSink::new(id.channel_on(webview.clone())));
    let response = dispatcher.dispatch(caller, request, event_sink).await;
    Ok(response.into_wire())
}

fn is_attach_request(request: &IpcValue) -> bool {
    let IpcValue::Object(fields) = request else {
        return false;
    };
    matches!(fields.get("kind"), Some(IpcValue::String(kind)) if kind == ATTACH_REQUEST_KIND)
}
