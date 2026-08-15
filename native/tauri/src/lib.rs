mod commands;

use std::{future::Future, pin::Pin, sync::Arc};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{ipc::Channel, plugin::TauriPlugin, Manager, Runtime, WebviewWindow};

/// Full frontend command used by `unified-ble-manager/tauri`.
pub const PLUGIN_COMMAND: &str = "plugin:unified-ble-manager|invoke";

/// Host-authenticated invocation facts. These are derived from Tauri and are
/// never deserialized from the untrusted webview request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthenticatedCaller {
    pub app_identifier: String,
    pub window_label: String,
}

impl AuthenticatedCaller {
    pub(crate) fn from_window<R: Runtime>(window: &WebviewWindow<R>) -> Self {
        Self {
            app_identifier: window.app_handle().config().identifier.clone(),
            window_label: window.label().to_owned(),
        }
    }
}

pub type DispatchFuture<'a> = Pin<Box<dyn Future<Output = Value> + Send + 'a>>;

/// Framework-neutral command authority implemented by the native BLE host.
/// It receives only authenticated caller facts plus copied IPC data.
pub trait IpcDispatcher: Send + Sync + 'static {
    fn dispatch<'a>(
        &'a self,
        caller: AuthenticatedCaller,
        request: Value,
        event_channel: Channel<Value>,
    ) -> DispatchFuture<'a>;
}

pub struct PluginState {
    dispatcher: Arc<dyn IpcDispatcher>,
}

impl PluginState {
    pub(crate) fn dispatcher(&self) -> Arc<dyn IpcDispatcher> {
        Arc::clone(&self.dispatcher)
    }
}

/// Constructs the Tauri plugin around a native dispatcher. Physical backends
/// implement `IpcDispatcher`; the plugin itself does not define a second BLE API.
pub struct PluginBuilder {
    dispatcher: Arc<dyn IpcDispatcher>,
}

impl PluginBuilder {
    pub fn new<Dispatcher>(dispatcher: Dispatcher) -> Self
    where
        Dispatcher: IpcDispatcher,
    {
        Self {
            dispatcher: Arc::new(dispatcher),
        }
    }

    pub fn build<R: Runtime>(self) -> TauriPlugin<R> {
        let dispatcher = self.dispatcher;
        tauri::plugin::Builder::new("unified-ble-manager")
            .invoke_handler(tauri::generate_handler![commands::invoke])
            .setup(move |app, _api| {
                let _ = app.manage(PluginState {
                    dispatcher: Arc::clone(&dispatcher),
                });
                Ok(())
            })
            .build()
    }
}
