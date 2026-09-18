use super::internal::{
    CoreBluetoothEvent, CoreBluetoothMessage, CoreBluetoothReply, CoreBluetoothReplyFuture,
    run_corebluetooth_thread,
};
use super::peripheral::{Peripheral, PeripheralId};
use crate::api::{Central, CentralEvent, CentralState, ScanFilter};
use crate::common::adapter_manager::AdapterManager;
use crate::{Error, Result};
use async_trait::async_trait;
use futures::channel::mpsc::{self, Sender};
use futures::sink::SinkExt;
use futures::stream::{Stream, StreamExt};
use log::*;
use objc2_core_bluetooth::CBManagerState;
use std::pin::Pin;
use std::sync::Arc;
use tokio::task;

/// Implementation of [api::Central](crate::api::Central).
#[derive(Clone, Debug)]
pub struct Adapter {
    manager: Arc<AdapterManager<Peripheral>>,
    sender: Sender<CoreBluetoothMessage>,
}

// UBM patch (UBM_PATCHES.md #7): resetting, unsupported and unauthorized
// are reported as themselves (upstream folded them into `Unknown`).
fn get_central_state(state: CBManagerState) -> CentralState {
    match state {
        CBManagerState::PoweredOn => CentralState::PoweredOn,
        CBManagerState::PoweredOff => CentralState::PoweredOff,
        CBManagerState::Resetting => CentralState::Resetting,
        CBManagerState::Unsupported => CentralState::Unsupported,
        CBManagerState::Unauthorized => CentralState::Unauthorized,
        _ => CentralState::Unknown,
    }
}

impl Adapter {
    pub(crate) async fn new() -> Result<Self> {
        let (sender, mut receiver) = mpsc::channel(256);
        let adapter_sender = run_corebluetooth_thread(sender)?;
        // Since init currently blocked until the state update, we know the
        // receiver is dropped after that. We can pick it up here and make it
        // part of our event loop to update our peripherals.
        debug!("Waiting on adapter connect");
        if !matches!(
            receiver.next().await,
            Some(CoreBluetoothEvent::DidUpdateState { state: _ })
        ) {
            return Err(Error::Other(
                "Adapter failed to connect.".to_string().into(),
            ));
        }
        debug!("Adapter connected");
        let manager = Arc::new(AdapterManager::default());

        let manager_clone = manager.clone();
        let adapter_sender_clone = adapter_sender.clone();
        task::spawn(async move {
            while let Some(msg) = receiver.next().await {
                match msg {
                    CoreBluetoothEvent::DeviceDiscovered {
                        uuid,
                        local_name,
                        advertisement_name,
                        event_receiver,
                    } => {
                        // UBM patch (UBM_PATCHES.md #19): a peripheral
                        // retrieved again replaces the entry it supersedes
                        // (upstream asserted it was new).
                        manager_clone.replace_peripheral(Peripheral::new(
                            uuid,
                            local_name,
                            advertisement_name,
                            Arc::downgrade(&manager_clone),
                            event_receiver,
                            adapter_sender_clone.clone(),
                        ));
                        manager_clone.emit(CentralEvent::DeviceDiscovered(uuid.into()));
                    }
                    CoreBluetoothEvent::DeviceUpdated {
                        uuid,
                        local_name,
                        advertisement_name,
                    } => {
                        let id = uuid.into();
                        if let Some(entry) = manager_clone.peripheral_mut(&id) {
                            entry.value().update_name(local_name, advertisement_name);
                            manager_clone.emit(CentralEvent::DeviceUpdated(id));
                        }
                    }
                    CoreBluetoothEvent::DeviceDisconnected { uuid } => {
                        manager_clone.emit(CentralEvent::DeviceDisconnected(uuid.into()));
                    }
                    CoreBluetoothEvent::Advertised { uuid, report } => {
                        manager_clone.emit(CentralEvent::Advertisement {
                            id: uuid.into(),
                            report,
                        });
                    }
                    CoreBluetoothEvent::DidUpdateState { state } => {
                        let central_state = get_central_state(state);
                        manager_clone.emit(CentralEvent::StateUpdate(central_state));
                    }
                }
            }
        });

        Ok(Adapter {
            manager,
            sender: adapter_sender,
        })
    }
}

#[async_trait]
impl Central for Adapter {
    type Peripheral = Peripheral;

    async fn events(&self) -> Result<Pin<Box<dyn Stream<Item = CentralEvent> + Send>>> {
        Ok(self.manager.event_stream())
    }

    async fn start_scan(&self, filter: ScanFilter) -> Result<()> {
        // UBM patch (UBM_PATCHES.md #8): the start is answered by the
        // CoreBluetooth thread, which refuses it unless the manager is
        // powered on (CoreBluetooth ignores a scan requested in any other
        // state, with no error). Upstream returned `Ok` once the request was
        // queued.
        let fut = CoreBluetoothReplyFuture::default();
        self.sender
            .to_owned()
            .send(CoreBluetoothMessage::StartScanning {
                filter,
                future: fut.get_state_clone(),
            })
            .await?;
        match fut.await {
            CoreBluetoothReply::Ok => Ok(()),
            CoreBluetoothReply::Err(detail) => Err(Error::Other(detail.into())),
            CoreBluetoothReply::Failed(error) => Err(Error::Platform(error)),
            reply => Err(Error::Other(
                format!("unexpected reply to a scan start: {reply:?}").into(),
            )),
        }
    }

    async fn stop_scan(&self) -> Result<()> {
        self.sender
            .to_owned()
            .send(CoreBluetoothMessage::StopScanning)
            .await?;
        Ok(())
    }

    async fn peripherals(&self) -> Result<Vec<Peripheral>> {
        Ok(self.manager.peripherals())
    }

    async fn peripheral(&self, id: &PeripheralId) -> Result<Peripheral> {
        self.manager.peripheral(id).ok_or(Error::DeviceNotFound)
    }

    /// UBM patch (UBM_PATCHES.md #19, finding 127): the peripheral with
    /// this identifier, retrieved from CoreBluetooth when this adapter no
    /// longer holds it (`retrievePeripheralsWithIdentifiers`, as the legacy
    /// addon reconnected without a scan).
    async fn add_peripheral(&self, id: &PeripheralId) -> Result<Peripheral> {
        if let Some(peripheral) = self.manager.peripheral(id) {
            return Ok(peripheral);
        }
        let fut = CoreBluetoothReplyFuture::default();
        self.sender
            .to_owned()
            .send(CoreBluetoothMessage::ResolvePeripheral {
                peripheral_uuid: id.uuid(),
                future: fut.get_state_clone(),
            })
            .await?;
        match fut.await {
            CoreBluetoothReply::Ok => {}
            CoreBluetoothReply::Err(_) => return Err(Error::DeviceNotFound),
            CoreBluetoothReply::Failed(error) => return Err(Error::Platform(error)),
            reply => {
                return Err(Error::Other(
                    format!("unexpected reply to a peripheral lookup: {reply:?}").into(),
                ));
            }
        }
        // The adapter task registers the retrieved peripheral in order after
        // the reply; wait for it briefly rather than race it.
        for _ in 0..100 {
            if let Some(peripheral) = self.manager.peripheral(id) {
                return Ok(peripheral);
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        Err(Error::DeviceNotFound)
    }

    async fn clear_peripherals(&self) -> Result<()> {
        self.manager.clear_peripherals();
        Ok(())
    }

    async fn adapter_info(&self) -> Result<String> {
        // TODO: Get information about the adapter.
        Ok("CoreBluetooth".to_string())
    }

    async fn adapter_state(&self) -> Result<CentralState> {
        let fut = CoreBluetoothReplyFuture::default();
        self.sender
            .to_owned()
            .send(CoreBluetoothMessage::GetAdapterState {
                future: fut.get_state_clone(),
            })
            .await?;

        match fut.await {
            CoreBluetoothReply::AdapterState(state) => {
                let central_state = get_central_state(state);
                return Ok(central_state.clone());
            }
            _ => panic!("Shouldn't get anything but a AdapterState!"),
        }
    }
}
