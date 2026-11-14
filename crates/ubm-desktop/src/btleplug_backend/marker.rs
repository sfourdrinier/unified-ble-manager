// crates/ubm-desktop/src/btleplug_backend/marker.rs
//! Exact retained native source marker/write receipts.

use super::*;
use crate::boundary::{
    OrderedMarkerWritePhase as Phase, OrderedMarkerWriteRequest,
    OrderedMarkerWriteStatus as Status, OrderedSourceLease,
};
use btleplug::platform::{
    SourceGroupHandle, SourceUnresolvedWriteRequest, SourceWriteRequest, SourceWriteSelector,
    SourceWriteStatus,
};

#[derive(Clone)]
pub(super) struct MarkerOwner {
    lease: OrderedSourceLease,
    request: OrderedMarkerWriteRequest,
    native: SourceUnresolvedWriteRequest,
    group: SourceGroupHandle,
    pub(super) retired: bool,
    preflight_error: Option<DesktopError>,
}

fn failure(detail: impl Into<String>) -> DesktopError {
    DesktopError::new(
        BleErrorCode::PlatformFailure,
        BleErrorDomain::Gatt,
        "gatt.ordered.marker-write",
    )
    .with_detail(detail.into())
}
fn same_source(a: &OrderedSourceLease, b: &OrderedSourceLease) -> bool {
    a.peer_id == b.peer_id
        && a.collection_id == b.collection_id
        && a.connection_generation == b.connection_generation
        && a.source_generation == b.source_generation
        && a.native_group == b.native_group
        && a.ingress == b.ingress
}
fn native_error(error: btleplug::PlatformError) -> DesktopError {
    map_radio(
        "gatt.ordered.marker-write",
        BleErrorCode::PlatformFailure,
        BleErrorDomain::Gatt,
    )(btleplug::Error::Platform(error))
}
fn status(value: SourceWriteStatus) -> Status {
    match value {
        SourceWriteStatus::Pending => Status::Pending,
        SourceWriteStatus::NotAdmitted(error) => {
            if error == btleplug::platform::SourceGroupError::Cancelled {
                return Status::NotAdmitted(Some(DesktopError::cancelled(
                    "gatt.ordered.marker-write",
                )));
            }
            let code = match error {
                btleplug::platform::SourceGroupError::TargetNotFound => BleErrorCode::GattNotFound,
                btleplug::platform::SourceGroupError::StaleGeneration => {
                    BleErrorCode::GattStaleHandle
                }
                _ => BleErrorCode::PlatformFailure,
            };
            Status::NotAdmitted(Some(
                DesktopError::new(code, BleErrorDomain::Gatt, "gatt.ordered.marker-write")
                    .with_detail(format!("native marker not admitted: {error:?}")),
            ))
        }
        SourceWriteStatus::Rejected(error) => {
            Status::Rejected(failure(format!("native marker rejected: {error:?}")))
        }
        SourceWriteStatus::Admitted { ordinal } => Status::Admitted {
            ordinal,
            write: Phase::Initiating,
        },
        SourceWriteStatus::Started { ordinal } => Status::Admitted {
            ordinal,
            write: Phase::Started(None),
        },
        SourceWriteStatus::InitiationFailed { ordinal, error } => Status::Admitted {
            ordinal,
            write: Phase::NotStarted(Some(native_error(error))),
        },
        SourceWriteStatus::Settled { ordinal, result } => Status::Admitted {
            ordinal,
            write: Phase::Settled(result.map_err(native_error)),
        },
    }
}

impl BtleplugRadio {
    async fn capture_marker_owner(
        &self,
        lease: &OrderedSourceLease,
        request: &OrderedMarkerWriteRequest,
    ) -> Result<(MarkerOwner, btleplug::platform::Peripheral, bool), DesktopError> {
        if request.target.0 != lease.peer_id
            || request.connection_generation != lease.connection_generation
        {
            return Err(failure("marker source or connection does not match"));
        }
        let owner = self.ordered_source_owner(&lease.peer_id, lease.collection_id)?;
        let matches = owner
            .lease
            .lock()
            .expect("ordered source lease")
            .as_ref()
            .is_some_and(|actual| same_source(actual, lease));
        if !matches {
            return Err(failure("marker source lease is stale"));
        }
        let group = owner
            .group
            .lock()
            .expect("ordered source group")
            .clone()
            .ok_or_else(|| failure("marker source group is unavailable"))?;
        let (peripheral, handle) = {
            let group = group.lock().await;
            if group.native_group != lease.native_group
                || Some(group.generation)
                    != super::desktop_source_generation(&lease.source_generation).ok()
            {
                return Err(failure("marker source generation is stale"));
            }
            (group.peripheral.clone(), group.handle.clone())
        };
        let identity = (
            request.snapshot_identity.owner.clone(),
            request.snapshot_identity.attachment,
            request.snapshot_identity.revision,
        );
        let native = SourceUnresolvedWriteRequest {
            id: request.marker_id,
            graph: btleplug::platform::marker_graph_identity(&identity),
            selector: SourceWriteSelector {
                peer_id: request.target.0.clone(),
                connection_generation: request.connection_generation.clone(),
                service_uuid: request.target.1.clone(),
                service_occurrence: request.target.2,
                characteristic_uuid: request.target.3.clone(),
                characteristic_occurrence: request.target.4,
            },
            value: request.value.clone(),
            write_type: if request.with_response {
                btleplug::api::WriteType::WithResponse
            } else {
                btleplug::api::WriteType::WithoutResponse
            },
        };
        let mut owners = self.marker_requests.lock().expect("marker owners");
        let key = (lease.collection_id, request.marker_id);
        if let Some(existing) = owners.get(&key) {
            if !same_source(&existing.lease, lease)
                || existing.request != *request
                || existing.native != native
            {
                return Err(failure("marker request identity differs"));
            }
            return Ok((existing.clone(), peripheral, false));
        }
        owners
            .retain(|(collection, _), owner| *collection != lease.collection_id || !owner.retired);
        let owner = MarkerOwner {
            lease: lease.clone(),
            request: request.clone(),
            native,
            group: handle,
            retired: false,
            preflight_error: None,
        };
        owners.insert(key, owner.clone());
        Ok((owner, peripheral, true))
    }

    pub(super) async fn start_native_marker_write(
        &self,
        lease: &OrderedSourceLease,
        request: &OrderedMarkerWriteRequest,
    ) -> Result<Status, DesktopError> {
        let (owner, peripheral, fresh) = self.capture_marker_owner(lease, request).await?;
        if !fresh {
            return owner
                .group
                .replay_unresolved_marker_write(&owner.native)
                .map(|receipt| receipt.map_or(Status::Pending, |receipt| status(receipt.status())))
                .map_err(|error| failure(format!("marker receipt lookup failed: {error:?}")));
        }
        let native = owner.native;
        let handle = owner.group;
        let identity = (
            request.snapshot_identity.owner.clone(),
            request.snapshot_identity.attachment,
            request.snapshot_identity.revision,
        );
        let characteristic = match Self::find_characteristic(
            &peripheral,
            &request.target.1,
            request.target.2,
            &request.target.3,
            request.target.4,
        ) {
            Some(value) => value,
            None => {
                let refusal = match self.gatt_snapshot_identity(&lease.peer_id) {
                    Ok(Some(current)) if current == request.snapshot_identity => {
                        btleplug::platform::SourceGroupError::TargetNotFound
                    }
                    Ok(_) => btleplug::platform::SourceGroupError::StaleGeneration,
                    Err(error) => {
                        if let Some(owner) = self
                            .marker_requests
                            .lock()
                            .expect("marker owners")
                            .get_mut(&(lease.collection_id, request.marker_id))
                        {
                            owner.preflight_error = Some(error);
                        }
                        btleplug::platform::SourceGroupError::SourceStateUnavailable
                    }
                };
                return handle
                    .record_unresolved_marker_not_admitted(native, refusal)
                    .map(|receipt| {
                        let mapped = status(receipt.status());
                        let owners = self.marker_requests.lock().expect("marker owners");
                        match (
                            mapped,
                            owners
                                .get(&(lease.collection_id, request.marker_id))
                                .and_then(|owner| owner.preflight_error.clone()),
                        ) {
                            (Status::NotAdmitted(_), Some(error)) => {
                                Status::NotAdmitted(Some(error))
                            }
                            (other, _) => other,
                        }
                    })
                    .map_err(|error| failure(format!("cannot retain marker refusal: {error:?}")));
            }
        };
        let resolved = SourceWriteRequest {
            id: native.id,
            graph: native.graph.clone(),
            selector: native.selector.clone(),
            route: btleplug::ubm_peer_notification_ingress::NotificationRoute {
                service_uuid: characteristic.service_uuid,
                service_instance: characteristic.service_instance,
                characteristic_uuid: characteristic.uuid,
                characteristic_instance: characteristic.instance,
            },
            value: native.value.clone(),
            write_type: native.write_type,
        };
        match peripheral
            .start_ordered_marker_write(&handle, &identity, &characteristic, resolved)
            .await
        {
            Ok(receipt) => Ok(status(receipt.status())),
            Err(btleplug::platform::SourceGroupError::SubmissionUncertain) => Ok(Status::Pending),
            Err(error) => Err(failure(format!(
                "native marker admission failed: {error:?}"
            ))),
        }
    }

    pub(super) async fn cancel_native_marker_write(
        &self,
        lease: &OrderedSourceLease,
        request: &OrderedMarkerWriteRequest,
    ) -> Result<Status, DesktopError> {
        let (owner, _, _) = self.capture_marker_owner(lease, request).await?;
        owner
            .group
            .cancel_unresolved_marker_write(&owner.native)
            .map_err(|error| failure(format!("native marker cancellation failed: {error:?}")))?;
        self.native_marker_write_status(lease, request)
    }

    pub(super) fn native_marker_write_status(
        &self,
        lease: &OrderedSourceLease,
        request: &OrderedMarkerWriteRequest,
    ) -> Result<Status, DesktopError> {
        let owners = self.marker_requests.lock().expect("marker owners");
        let owner = owners
            .get(&(lease.collection_id, request.marker_id))
            .ok_or_else(|| failure("marker owner is unavailable"))?;
        if !same_source(&owner.lease, lease) || owner.request != *request {
            return Err(failure("marker request identity differs"));
        }
        owner
            .group
            .replay_unresolved_marker_write(&owner.native)
            .map(|receipt| {
                let mapped = receipt.map_or(Status::Pending, |receipt| status(receipt.status()));
                match (mapped, owner.preflight_error.clone()) {
                    (Status::NotAdmitted(_), Some(error)) => Status::NotAdmitted(Some(error)),
                    (other, _) => other,
                }
            })
            .map_err(|e| failure(format!("marker receipt lookup failed: {e:?}")))
    }
    pub(super) fn forget_native_marker_write(
        &self,
        lease: &OrderedSourceLease,
        request: &OrderedMarkerWriteRequest,
    ) -> Result<(), DesktopError> {
        let mut owners = self.marker_requests.lock().expect("marker owners");
        let key = (lease.collection_id, request.marker_id);
        let owner = owners
            .get_mut(&key)
            .ok_or_else(|| failure("marker owner is unavailable"))?;
        if !same_source(&owner.lease, lease) || owner.request != *request {
            return Err(failure("marker request identity differs"));
        }
        owner
            .group
            .forget_marker_write(request.marker_id)
            .map_err(|e| failure(format!("marker receipt retirement failed: {e:?}")))?;
        owner.retired = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use btleplug::platform::SourceGroupError;

    #[test]
    fn native_marker_status_preserves_admission_and_indeterminate_start() {
        assert!(matches!(
            status(SourceWriteStatus::Pending),
            Status::Pending
        ));
        assert!(matches!(
            status(SourceWriteStatus::NotAdmitted(
                SourceGroupError::StaleGeneration
            )),
            Status::NotAdmitted(Some(_))
        ));
        assert!(matches!(
            status(SourceWriteStatus::Rejected(
                SourceGroupError::OrdinalExhausted
            )),
            Status::Rejected(_)
        ));
        assert!(matches!(
            status(SourceWriteStatus::Admitted { ordinal: 7 }),
            Status::Admitted {
                ordinal: 7,
                write: Phase::Initiating
            }
        ));
        assert!(matches!(
            status(SourceWriteStatus::Settled {
                ordinal: 8,
                result: Ok(())
            }),
            Status::Admitted {
                ordinal: 8,
                write: Phase::Settled(Ok(()))
            }
        ));
    }
}
