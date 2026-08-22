use std::collections::BTreeMap;

use serde_json::Value;
use tauri::{
    ipc::{CommandScope, JavaScriptChannelId},
    Runtime, State, Webview, WebviewWindow,
};

use crate::{
    security_scope::SecurityPermission, AuthenticatedCaller, IpcEventSink, IpcValue, PluginState,
    ATTACH_REQUEST_KIND,
};

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
    security_scope: CommandScope<SecurityPermission>,
) -> Result<Value, String> {
    let caller = AuthenticatedCaller::from_window(&window);
    let dispatcher = state.dispatcher();
    let request = IpcValue::from_wire(request)?;
    if let Some(permission) = security_permission_for(&request) {
        let allowed = security_scope
            .allows()
            .iter()
            .any(|scope| **scope == permission);
        let denied = security_scope
            .denies()
            .iter()
            .any(|scope| **scope == permission);
        if !allowed || denied {
            return Ok(security_permission_denied(permission).into_wire());
        }
    }
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

fn security_permission_for(request: &IpcValue) -> Option<SecurityPermission> {
    let IpcValue::Object(fields) = request else {
        return None;
    };
    if !matches!(fields.get("kind"), Some(IpcValue::String(kind)) if kind == "route") {
        return None;
    }
    let IpcValue::Object(envelope) = fields.get("envelope")? else {
        return None;
    };
    let IpcValue::String(command) = envelope.get("command")? else {
        return None;
    };
    match command.as_str() {
        "security.state" => Some(SecurityPermission::State),
        "security.pair" => Some(SecurityPermission::Pair),
        "security.cancel-pairing" => Some(SecurityPermission::CancelPairing),
        "security.unpair" => Some(SecurityPermission::Unpair),
        "security.custom-ceremony" => Some(SecurityPermission::CustomCeremony),
        _ => None,
    }
}

fn security_permission_denied(permission: SecurityPermission) -> IpcValue {
    let mut error = BTreeMap::new();
    error.insert(
        "code".to_owned(),
        IpcValue::String("permission.denied".to_owned()),
    );
    error.insert("domain".to_owned(), IpcValue::String("ipc".to_owned()));
    error.insert(
        "operation".to_owned(),
        IpcValue::String(format!("tauri.security.{}", permission.operation_name())),
    );
    error.insert("platform".to_owned(), IpcValue::Null);
    error.insert(
        "retryability".to_owned(),
        IpcValue::String("never".to_owned()),
    );
    IpcValue::Object(BTreeMap::from([
        ("kind".to_owned(), IpcValue::String("failure".to_owned())),
        ("error".to_owned(), IpcValue::Object(error)),
    ]))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::security_permission_for;
    use crate::{security_scope::SecurityPermission, IpcValue};

    fn route(command: &str) -> IpcValue {
        IpcValue::Object(BTreeMap::from([
            ("kind".to_owned(), IpcValue::String("route".to_owned())),
            (
                "envelope".to_owned(),
                IpcValue::Object(BTreeMap::from([(
                    "command".to_owned(),
                    IpcValue::String(command.to_owned()),
                )])),
            ),
        ]))
    }

    #[test]
    fn security_commands_map_to_independent_permission_atoms() {
        assert_eq!(
            security_permission_for(&route("security.state")),
            Some(SecurityPermission::State)
        );
        assert_eq!(
            security_permission_for(&route("security.pair")),
            Some(SecurityPermission::Pair)
        );
        assert_eq!(
            security_permission_for(&route("security.cancel-pairing")),
            Some(SecurityPermission::CancelPairing)
        );
        assert_eq!(
            security_permission_for(&route("security.unpair")),
            Some(SecurityPermission::Unpair)
        );
        assert_eq!(
            security_permission_for(&route("security.custom-ceremony")),
            Some(SecurityPermission::CustomCeremony)
        );
    }

    #[test]
    fn unknown_commands_and_renderer_scope_fields_do_not_grant_security_permission() {
        assert_eq!(security_permission_for(&route("security.wildcard")), None);
        assert_eq!(
            security_permission_for(&IpcValue::Object(BTreeMap::from([
                ("kind".to_owned(), IpcValue::String("route".to_owned())),
                (
                    "securityScope".to_owned(),
                    IpcValue::String("unpair".to_owned())
                ),
            ]))),
            None
        );
    }
}
