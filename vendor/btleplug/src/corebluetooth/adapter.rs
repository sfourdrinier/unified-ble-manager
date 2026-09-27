use super::internal::{
    CoreBluetoothEvent, CoreBluetoothMessage, CoreBluetoothReply, CoreBluetoothReplyFuture,
    run_corebluetooth_thread,
};

#[cfg(test)]
mod directory_tests {
    use super::*;
    use futures::FutureExt;

    #[tokio::test]
    async fn directory_closed_receiver_answers_instead_of_losing_reply() {
        let (sender, receiver) = mpsc::channel(8);
        drop(receiver);
        let future = CoreBluetoothReplyFuture::default();
        let state = future.get_state_clone();
        super::super::internal::enqueue_directory_event(
            &sender,
            CoreBluetoothEvent::DirectoryReady {
                peers: Vec::new(),
                future: state.clone(),
            },
            &state,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(future.await, CoreBluetoothReply::Failed(error) if error.code == "directory-queue-closed")
        );
    }

    #[tokio::test]
    async fn directory_receiver_failure_after_registration_never_reports_partial_success() {
        let (sender, mut receiver) = mpsc::channel(8);
        let future = CoreBluetoothReplyFuture::default();
        let state = future.get_state_clone();
        let (_, notifications) = mpsc::channel(1);
        super::super::internal::enqueue_directory_event(
            &sender,
            CoreBluetoothEvent::RetrievedPeripheral {
                uuid: uuid::Uuid::nil(),
                local_name: None,
                event_receiver: notifications,
            },
            &state,
        )
        .await
        .unwrap();
        assert!(matches!(
            receiver.next().await,
            Some(CoreBluetoothEvent::RetrievedPeripheral { .. })
        ));
        drop(receiver);
        super::super::internal::enqueue_directory_event(
            &sender,
            CoreBluetoothEvent::DirectoryReady {
                peers: vec![(uuid::Uuid::nil(), None)],
                future: state.clone(),
            },
            &state,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(future.await, CoreBluetoothReply::Failed(error) if error.code == "directory-queue-closed")
        );
    }

    #[tokio::test]
    async fn directory_reply_registers_same_identity_without_radio_or_discovery_event() {
        let manager = Arc::new(AdapterManager::default());
        let (sender, mut requests) = mpsc::channel(8);
        let adapter = Adapter {
            manager: manager.clone(),
            sender: sender.clone(),
        };
        let id = uuid::Uuid::parse_str("00e2ce71-3ba4-6569-e3de-3081ce0c95fb").unwrap();
        let mut events = adapter.events().await.unwrap();
        let work = adapter.directory_peers(Some(vec![id]), None);
        let answer = async {
            let CoreBluetoothMessage::DirectoryLookup {
                services,
                identifier,
                future,
            } = requests.next().await.unwrap()
            else {
                panic!("lookup must not connect")
            };
            assert_eq!(services, Some(vec![id]));
            assert_eq!(identifier, None);
            let (_notification_sender, notifications) = mpsc::channel(8);
            let (mut queued, mut received) = mpsc::channel(8);
            queued
                .send(CoreBluetoothEvent::RetrievedPeripheral {
                    uuid: id,
                    local_name: Some("SIM".to_owned()),
                    event_receiver: notifications,
                })
                .await
                .unwrap();
            queued
                .send(CoreBluetoothEvent::DirectoryReady {
                    peers: vec![(id, Some("SIM".to_owned()))],
                    future,
                })
                .await
                .unwrap();
            drop(queued);
            while let Some(event) = received.next().await {
                assert!(handle_directory_event(event, &manager, &sender).is_none());
            }
        };
        let (result, ()) = tokio::join!(work, answer);
        assert_eq!(result.unwrap(), vec![(id, Some("SIM".to_owned()))]);
        assert!(adapter.peripheral(&id.into()).await.is_ok());
        // Subsequent normal connect uses this exact public handle. Lookup itself
        // has emitted no radio command or fabricated advertisement/link event.
        assert!(requests.next().now_or_never().is_none());
        assert!(events.next().now_or_never().is_none());
    }
}
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

fn register_directory_peripheral(
    manager: &Arc<AdapterManager<Peripheral>>,
    sender: &Sender<CoreBluetoothMessage>,
    uuid: uuid::Uuid,
    local_name: Option<String>,
    event_receiver: mpsc::Receiver<super::internal::PeripheralEventInternal>,
) {
    manager.replace_peripheral(Peripheral::new(
        uuid,
        local_name,
        None,
        Arc::downgrade(manager),
        event_receiver,
        sender.clone(),
    ));
}

fn handle_directory_event(
    event: CoreBluetoothEvent,
    manager: &Arc<AdapterManager<Peripheral>>,
    sender: &Sender<CoreBluetoothMessage>,
) -> Option<CoreBluetoothEvent> {
    match event {
        CoreBluetoothEvent::DirectoryReady { peers, future } => {
            future
                .lock()
                .unwrap()
                .set_reply(CoreBluetoothReply::DirectoryPeers(peers));
            None
        }
        CoreBluetoothEvent::RetrievedPeripheral {
            uuid,
            local_name,
            event_receiver,
        } => {
            register_directory_peripheral(manager, sender, uuid, local_name, event_receiver);
            None
        }
        event => Some(event),
    }
}

impl Adapter {
    /// Read-only OS directory lookup on this adapter's existing manager.
    pub async fn directory_peers(
        &self,
        services: Option<Vec<uuid::Uuid>>,
        identifier: Option<uuid::Uuid>,
    ) -> Result<Vec<(uuid::Uuid, Option<String>)>> {
        let future = CoreBluetoothReplyFuture::default();
        self.sender
            .clone()
            .send(CoreBluetoothMessage::DirectoryLookup {
                services,
                identifier,
                future: future.get_state_clone(),
            })
            .await?;
        match future.await {
            CoreBluetoothReply::DirectoryPeers(peers) => Ok(peers),
            CoreBluetoothReply::Failed(error) => Err(Error::Platform(error)),
            reply => Err(Error::Other(
                format!("unexpected directory reply: {reply:?}").into(),
            )),
        }
    }

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
                let Some(msg) = handle_directory_event(msg, &manager_clone, &adapter_sender_clone)
                else {
                    continue;
                };
                match msg {
                    CoreBluetoothEvent::DirectoryReady { .. }
                    | CoreBluetoothEvent::RetrievedPeripheral { .. } => {
                        unreachable!("directory events handled above")
                    }
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
        self.directory_peers(None, Some(id.uuid())).await?;
        self.manager.peripheral(id).ok_or(Error::DeviceNotFound)
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
