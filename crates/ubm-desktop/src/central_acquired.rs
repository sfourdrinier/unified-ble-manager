//! Native acquired handles share GATT admission, deadlines and parent lifetime.
use super::*;
use crate::acquired_gatt::{
    AcquisitionKind,
    ownership::{Admission, Resource},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquiredGattHandle {
    pub handle: String,
    pub mtu: u16,
    pub kind: AcquisitionKind,
}

impl<B: RadioBoundary> DesktopCentral<B> {
    async fn acquired_current(
        &self,
        resource: &Resource,
        ctl: &OpControl,
        operation: &'static str,
    ) -> Result<(), DesktopError> {
        resource.check()?;
        if resource.lease.as_deref() != ctl.connection_lease() {
            return Err(contract_error(
                BleErrorCode::OwnershipDenied,
                BleErrorDomain::Gatt,
                operation,
            ));
        }
        let admission = resource.admission.get().ok_or_else(|| {
            contract_error(BleErrorCode::StreamClosed, BleErrorDomain::Gatt, operation)
        })?;
        let peer_key = self.known_peer_key(&resource.peer).await?;
        let core = self.inner.core.lock().await;
        let current = Generations::of(&core, &peer_key);
        if current.connection != admission.connection || current.database != admission.database {
            return Err(contract_error(
                BleErrorCode::GattStaleHandle,
                BleErrorDomain::Gatt,
                operation,
            ));
        }
        let (index, _, _) = self.resolve_instance(
            &core,
            &peer_key,
            &resource.peer,
            &admission.selector,
            operation,
            false,
        )?;
        core.validate_gatt_admission(ctl.gatt_path(index), operation)
            .map_err(DesktopError::from)
    }

    pub async fn acquire_gatt(
        &self,
        peer: &str,
        selector: &PathSelector,
        kind: AcquisitionKind,
        ctl: OpControl,
    ) -> Result<AcquiredGattHandle, DesktopError> {
        let operation = match kind {
            AcquisitionKind::Write => "gatt.acquire-write",
            AcquisitionKind::Notify => "gatt.acquire-notify",
        };
        let _settle = SettleOnDrop(&ctl.ticket);
        self.precheck(&ctl, operation)?;
        let window = ctl.budget.window(LIVENESS_OP);
        self.validate_gatt_prerequisite(peer, selector, &ctl, operation, false)
            .await?;
        let _admission = self
            .wait_gatt_admission(peer, &ctl, operation, window)
            .await?;
        let peer_key = self.known_peer_key(peer).await?;
        if kind == AcquisitionKind::Notify {
            let scope = {
                let core = self.inner.core.lock().await;
                self.resolve_instance(&core, &peer_key, peer, selector, operation, false)?
                    .1
            };
            if self.inner.failed_disables.lock().await.contains(&scope) {
                return Err(contract_error(BleErrorCode::LifecycleInvalidState, BleErrorDomain::Core, operation)
                    .with_detail("ordinary CCCD removal remains owned; complete unsubscribe before acquiring its transport"));
            }
        }
        let opening = {
            let core = self.inner.core.lock().await;
            core.check_capability("gatt:high-throughput-acquire", operation)
                .map_err(DesktopError::from)?;
            let (index, scope, _) =
                self.resolve_instance(&core, &peer_key, peer, selector, operation, false)?;
            core.validate_gatt_admission(ctl.gatt_path(index), operation)
                .map_err(DesktopError::from)?;
            self.inner.acquired.assert_available(&scope, kind)?;
            if kind == AcquisitionKind::Notify && core.physical_cccd_enabled(index) {
                return Err(contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    operation,
                )
                .with_detail("ordinary notification enablement is still owned"));
            }
            let properties = core
                .stored_path(index)
                .map(|path| path.properties())
                .unwrap_or(0);
            let required = match kind {
                AcquisitionKind::Write => ubm_core::central::GATT_PROP_WRITE_NO_RESPONSE,
                AcquisitionKind::Notify => {
                    ubm_core::central::GATT_PROP_NOTIFY | ubm_core::central::GATT_PROP_INDICATE
                }
            };
            if properties & required == 0 {
                return Err(contract_error(
                    BleErrorCode::GattPropertyNotSupported,
                    BleErrorDomain::Gatt,
                    operation,
                ));
            }
            let opening = self
                .inner
                .acquired
                .reserve(peer, ctl.connection_lease(), kind)?;
            let current = Generations::of(&core, &peer_key);
            opening
                .resource
                .admission
                .set(Admission {
                    scope,
                    selector: selector.clone(),
                    connection: current.connection,
                    database: current.database,
                })
                .map_err(|_| {
                    contract_error(
                        BleErrorCode::ProtocolViolation,
                        BleErrorDomain::Gatt,
                        operation,
                    )
                })?;
            opening
        };
        let scope = {
            let core = self.inner.core.lock().await;
            self.resolve_instance(&core, &peer_key, peer, selector, operation, false)?
                .1
        };
        let work = async {
            tokio::select! {
                biased;
                error = opening.resource.ended() => Err(error),
                result = self.inner.boundary.acquire_gatt(&scope, kind) => result,
            }
        };
        let transport =
            match drive_link(&self.inner, peer, operation, &ctl.ticket, window, work).await {
                Wait::Done(result) => result?,
                Wait::Expired => return Err(timed_out(operation, window)),
                Wait::Cancelled => return Err(ctl.ticket.interruption(operation)),
            };
        let mtu = transport.mtu;
        let resource = opening.publish(transport).await?;
        let admission = match self.precheck(&ctl, operation) {
            Ok(()) => self.acquired_current(&resource, &ctl, operation).await,
            Err(error) => Err(error),
        };
        if let Err(primary) = admission {
            return match self.inner.acquired.close(&resource.handle).await {
                Ok(()) => Err(primary),
                Err(cleanup) => match crate::errors::cleanup_result(
                    "acquired-gatt-admission",
                    vec![primary, cleanup],
                ) {
                    Err(error) => Err(error),
                    Ok(()) => unreachable!("nonempty failures"),
                },
            };
        }
        Ok(AcquiredGattHandle {
            handle: resource.handle.clone(),
            mtu,
            kind,
        })
    }

    pub async fn acquired_write(
        &self,
        handle: &str,
        value: Vec<u8>,
        ctl: OpControl,
    ) -> Result<(), DesktopError> {
        const OP: &str = "gatt.acquired-write";
        let _settle = SettleOnDrop(&ctl.ticket);
        self.precheck(&ctl, OP)?;
        let resource = self.inner.acquired.lookup(handle)?;
        if resource.kind != AcquisitionKind::Write {
            return Err(contract_error(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Gatt,
                OP,
            ));
        }
        self.acquired_current(&resource, &ctl, OP).await?;
        let window = ctl.budget.window(LIVENESS_OP);
        let _admission = self
            .wait_gatt_admission(&resource.peer, &ctl, OP, window)
            .await?;
        self.acquired_current(&resource, &ctl, OP).await?;
        let transport = resource.transport()?;
        if value.len() > usize::from(transport.mtu.saturating_sub(3)).min(512) {
            return Err(contract_error(
                BleErrorCode::BytesTooLarge,
                BleErrorDomain::Gatt,
                OP,
            ));
        }
        let work = async {
            tokio::select! { biased; error = resource.ended() => Err(error), result = transport.io.send(&value) => result }
        };
        match drive_link(&self.inner, &resource.peer, OP, &ctl.ticket, window, work).await {
            Wait::Done(result) => result.map_err(classify_dispatched_write),
            Wait::Expired => Err(classify_dispatched_write(timed_out(OP, window))),
            Wait::Cancelled => Err(classify_dispatched_write(ctl.ticket.interruption(OP))),
        }
    }

    pub async fn acquired_receive(
        &self,
        handle: &str,
        ctl: OpControl,
    ) -> Result<Vec<u8>, DesktopError> {
        const OP: &str = "gatt.acquired-receive";
        let _settle = SettleOnDrop(&ctl.ticket);
        self.precheck(&ctl, OP)?;
        let resource = self.inner.acquired.lookup(handle)?;
        if resource.kind != AcquisitionKind::Notify {
            return Err(contract_error(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Gatt,
                OP,
            ));
        }
        self.acquired_current(&resource, &ctl, OP).await?;
        let transport = resource.transport()?;
        // A subscribed stream's idle time is not an operation deadline.
        let window = ctl.budget.window_without_backstop();
        let work = async {
            tokio::select! { biased; error = resource.ended() => Err(error), result = transport.io.receive() => result }
        };
        match drive_link(&self.inner, &resource.peer, OP, &ctl.ticket, window, work).await {
            Wait::Done(result) => {
                let value = result?;
                self.acquired_current(&resource, &ctl, OP).await?;
                Ok(value)
            }
            Wait::Expired => Err(timed_out(OP, window)),
            Wait::Cancelled => Err(ctl.ticket.interruption(OP)),
        }
    }

    pub async fn close_acquired(&self, handle: &str, ctl: OpControl) -> Result<(), DesktopError> {
        const OP: &str = "gatt.acquired-close";
        let _settle = SettleOnDrop(&ctl.ticket);
        if let Ok(resource) = self.inner.acquired.lookup(handle)
            && resource.lease.as_deref() != ctl.connection_lease()
        {
            return Err(contract_error(
                BleErrorCode::OwnershipDenied,
                BleErrorDomain::Cleanup,
                OP,
            ));
        }
        let window = ctl.budget.window(LIVENESS_CLEANUP);
        match drive(&ctl.ticket, window, self.inner.acquired.close(handle)).await {
            Wait::Done(result) => result,
            Wait::Expired => Err(timed_out(OP, window)
                .with_detail("acquired transport cleanup remains owned for retry")),
            Wait::Cancelled => Err(ctl.ticket.interruption(OP)),
        }
    }
}
