mod btleplug_dispatcher;
mod capabilities;
mod commands;
mod scan_plan;
mod security_scope;
mod wire;

use std::{future::Future, pin::Pin, sync::Arc};

use serde::{Deserialize, Serialize};
use tauri::{
    plugin::TauriPlugin, webview::PageLoadEvent, Manager, RunEvent, Runtime, WebviewWindow,
    WindowEvent,
};

pub use btleplug_dispatcher::{BtleplugDispatcher, BtleplugDispatcherOptions};
pub use wire::{IpcEventSink, IpcValue, ATTACH_REQUEST_KIND};

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
    pub(crate) fn new(app_identifier: String, window_label: String) -> Self {
        Self {
            app_identifier,
            window_label,
        }
    }

    pub(crate) fn from_window<R: Runtime>(window: &WebviewWindow<R>) -> Self {
        Self::new(
            window.app_handle().config().identifier.clone(),
            window.label().to_owned(),
        )
    }
}

pub type DispatchFuture<'a> = Pin<Box<dyn Future<Output = IpcValue> + Send + 'a>>;

/// Framework-neutral command authority implemented by the native BLE host.
/// It receives only authenticated caller facts plus copied IPC data.
pub trait IpcDispatcher: Send + Sync + 'static {
    fn dispatch<'a>(
        &'a self,
        caller: AuthenticatedCaller,
        request: IpcValue,
        // `Some` only for the attach request, which binds the caller's event
        // sink for the lifetime of the attachment. See `commands::invoke`.
        event_sink: Option<IpcEventSink>,
    ) -> DispatchFuture<'a>;

    /// Revokes all leases and resources owned by this authenticated caller.
    ///
    /// Implementations must make the caller inadmissible before returning so
    /// a replacement document cannot race cleanup and inherit stale ownership.
    /// Slow native settlement may continue internally after that revocation.
    fn release_caller(&self, caller: AuthenticatedCaller);
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
        let setup_dispatcher = Arc::clone(&self.dispatcher);
        let page_dispatcher = Arc::clone(&self.dispatcher);
        let event_dispatcher = self.dispatcher;

        tauri::plugin::Builder::new("unified-ble-manager")
            .invoke_handler(tauri::generate_handler![commands::invoke])
            .setup(move |app, _api| {
                let _ = app.manage(PluginState {
                    dispatcher: Arc::clone(&setup_dispatcher),
                });
                Ok(())
            })
            .on_page_load(move |webview, payload| {
                if payload.event() == PageLoadEvent::Started {
                    let window = webview.window();
                    page_dispatcher.release_caller(AuthenticatedCaller::new(
                        webview.app_handle().config().identifier.clone(),
                        window.label().to_owned(),
                    ));
                }
            })
            .on_event(move |app, event| {
                if let RunEvent::WindowEvent { label, event, .. } = event {
                    if matches!(event, WindowEvent::Destroyed) {
                        event_dispatcher.release_caller(AuthenticatedCaller::new(
                            app.config().identifier.clone(),
                            label.clone(),
                        ));
                    }
                }
            })
            .build()
    }
}
