// crates/ubm-desktop/src/central/ordered_marker_write.rs
//! Retained exact marker/write transactions and downstream credit.

use super::*;
use crate::boundary::{
    OrderedMarkerWritePhase, OrderedMarkerWriteRequest, OrderedMarkerWriteStatus,
    OrderedSourceCleanup, OrderedSourceLease,
};

pub(super) struct MarkerTransaction {
    pub request: OrderedMarkerWriteRequest,
    pub lease: OrderedSourceLease,
    pub owner: Option<u64>,
    pub status: OrderedMarkerWriteStatus,
    pub error: Option<DesktopError>,
    pub intent: Arc<std::sync::atomic::AtomicU8>,
}

impl<B: RadioBoundary> DesktopCentral<B> {
    /// Validate an exact live recorder group and caller lease while retaining a
    /// peer/group admission permit through the caller's mapping transfer.
    pub async fn acquire_ordered_recording_reclaim(
        &self,
        peer_id: &str,
        collection_id: u64,
        token: u64,
        caller_lease: &str,
    ) -> Result<OrderedRecordingReclaimPermit, DesktopError> {
        const OP: &str = "gatt.ordered.recording-reclaim";
        // Admission order matches release: exact lease and peer connection,
        // then GATT FIFO, group, and GATT starting admission. Core and batch state locks are dropped
        // before native calls. Native acquire/commit/release must complete on
        // the source owner without reentering these gates; callbacks are held
        // in source order until release. A failed native release deliberately
        // retains these bounded admissions as visible cleanup debt.
        let lease_gate = {
            let mut gates = lock_std(&self.inner.lease_releases);
            gates.retain(|_, gate| gate.strong_count() != 0);
            let slot = gates
                .entry((peer_id.to_owned(), caller_lease.to_owned()))
                .or_default();
            if let Some(gate) = slot.upgrade() {
                gate
            } else {
                let gate = Arc::new(Mutex::new(None));
                *slot = Arc::downgrade(&gate);
                gate
            }
        };
        let lease =
            tokio::time::timeout(std::time::Duration::from_secs(5), lease_gate.lock_owned())
                .await
                .map_err(|_| {
                    DesktopError::new(BleErrorCode::OperationTimedOut, BleErrorDomain::Cleanup, OP)
                        .with_outcome(
                            Some(CommitState::NotDispatched),
                            Retryability::CallerDecides,
                        )
                })?;
        if lease.is_some() {
            return Err(contract_error(
                BleErrorCode::OwnershipDenied,
                BleErrorDomain::Connection,
                OP,
            ));
        }
        let connection_gate = {
            let mut gates = lock_std(&self.inner.connection_admission);
            Arc::clone(
                gates
                    .entry(peer_id.to_owned())
                    .or_insert_with(|| Arc::new(Mutex::new(()))),
            )
        };
        let connection_guard = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            connection_gate.lock_owned(),
        )
        .await
        .map_err(|_| {
            DesktopError::new(BleErrorCode::OperationTimedOut, BleErrorDomain::Cleanup, OP)
                .with_outcome(
                    Some(CommitState::NotDispatched),
                    Retryability::CallerDecides,
                )
        })?;
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(BleErrorCode::OwnershipDenied, BleErrorDomain::Gatt, OP)
            })?;
        let delivery_gate = Arc::clone(&owner.lock().await.delivery_lock);
        let (connection, database, identity) = {
            let state = owner.lock().await;
            super::ordered_batch_cleanup::check_recording_owner(
                &lock_std(&state.delivery_state),
                Some(token),
            )?;
            if state.closing
                || state.retired
                || state.cleanup_complete
                || state.receipt_acknowledged
            {
                return Err(contract_error(
                    BleErrorCode::GattStaleHandle,
                    BleErrorDomain::Gatt,
                    OP,
                ));
            }
            (
                state.connection_generation.clone(),
                state.database_generation.clone(),
                state.snapshot_identity.clone(),
            )
        };
        let connection = connection.ok_or_else(|| {
            contract_error(BleErrorCode::GattStaleHandle, BleErrorDomain::Gatt, OP)
        })?;
        let database = database.ok_or_else(|| {
            contract_error(BleErrorCode::GattStaleHandle, BleErrorDomain::Gatt, OP)
        })?;
        let identity = identity.ok_or_else(|| {
            contract_error(
                BleErrorCode::GattDiscoveryRequired,
                BleErrorDomain::Gatt,
                OP,
            )
        })?;
        let ctl = OpControl::budget_ms(5000);
        let window = ctl.budget.window(LIVENESS_OP);
        let admission = self.wait_gatt_admission(peer_id, &ctl, OP, window).await?;
        let (group, starting) = loop {
            let group = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                Arc::clone(&delivery_gate).lock_owned(),
            )
            .await
            .map_err(|_| {
                DesktopError::new(BleErrorCode::OperationTimedOut, BleErrorDomain::Cleanup, OP)
                    .with_outcome(
                        Some(CommitState::NotDispatched),
                        Retryability::CallerDecides,
                    )
            })?;
            {
                let state = owner.lock().await;
                let delivery = lock_std(&state.delivery_state);
                super::ordered_batch_cleanup::check_recording_owner(&delivery, Some(token))?;
                if state.closing
                    || state.retired
                    || state.cleanup_complete
                    || state.receipt_acknowledged
                    || state.connection_generation.as_ref() != Some(&connection)
                    || state.database_generation.as_ref() != Some(&database)
                    || state.snapshot_identity.as_ref() != Some(&identity)
                {
                    return Err(contract_error(
                        BleErrorCode::GattStaleHandle,
                        BleErrorDomain::Gatt,
                        OP,
                    ));
                }
                // A recorder may have consumed a delivery and still need this
                // gate to acknowledge it after durable commit. Pinning the
                // gate here would strand that ACK and hence stop behind the
                // caller's own retained reclaim permit.
                if delivery.pending.is_some() || delivery.pending_discard.is_some() {
                    return Err(contract_error(
                        BleErrorCode::LifecycleInvalidState,
                        BleErrorDomain::Cleanup,
                        OP,
                    )
                    .with_detail("recorder delivery acknowledgment is pending; retry reclaim")
                    .with_outcome(
                        Some(CommitState::NotDispatched),
                        Retryability::CallerDecides,
                    ));
                }
            }
            if let Some(starting) = admission.try_begin_starting()? {
                break (group, starting);
            }
            // A provisional lifecycle may need the group gate. Never wait
            // for its decision while holding that gate.
            drop(group);
            match drive_link(
                &self.inner,
                peer_id,
                OP,
                &ctl.ticket,
                window,
                admission.wait(),
            )
            .await
            {
                Wait::Done(result) => result?,
                Wait::Expired => return Err(timed_out(OP, window)),
                Wait::Cancelled => return Err(ctl.ticket.interruption(OP)),
            }
        };
        let peer_key = self.known_peer_key(peer_id).await?;
        let coordinator = discovery_coordinator(&self.inner, peer_id);
        let snapshot = coordinator.snapshot.lock().await;
        let core = self.inner.core.lock().await;
        if !core.lease_accepts_work(&peer_key, caller_lease)
            || core.connection_state(&peer_key) != Some(ConnectionState::Connected)
        {
            return Err(contract_error(
                BleErrorCode::OwnershipDenied,
                BleErrorDomain::Connection,
                OP,
            ));
        }
        if core.connection_generation(&peer_key).as_deref() != Some(&connection)
            || core.database_generation(&peer_key).as_deref() != Some(&database)
            || core.database_state(&peer_key) != Some(DatabaseState::Current)
            || snapshot.generations.as_ref() != Some(&(connection.clone(), database))
            || snapshot.identity.as_ref() != Some(&identity)
            || !snapshot.leases.contains(caller_lease)
            || self.inner.boundary.gatt_snapshot_identity(peer_id)? != Some(identity.clone())
        {
            return Err(contract_error(
                BleErrorCode::GattStaleHandle,
                BleErrorDomain::Gatt,
                OP,
            ));
        }
        if self
            .inner
            .boundary
            .ordered_reclaim_requires_snapshot_binding()
        {
            match snapshot.reclaim_binding.as_ref() {
                Some(Ok(())) => {}
                Some(Err(error)) => return Err(error.clone()),
                None => {
                    return Err(contract_error(
                        BleErrorCode::GattDiscoveryRequired,
                        BleErrorDomain::Gatt,
                        OP,
                    ));
                }
            }
        }
        drop(core);
        drop(snapshot);
        let slot = reserve_retirement(&self.inner).ok_or_else(|| {
            contract_error(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Cleanup,
                OP,
            )
        })?;
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let (completion_tx, completion_rx) = tokio::sync::watch::channel(None);
        let inner = Arc::clone(&self.inner);
        let task_inner = Arc::clone(&inner);
        {
            let mut tasks = lock_std(&inner.reclaim_tasks);
            if inner.shut_down.load(Ordering::SeqCst) {
                return Err(contract_error(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Cleanup,
                    OP,
                ));
            }
            tasks.retain(|task| !task.is_finished());
            let native_peer = peer_id.to_owned();
            inner.reclaim_active.fetch_add(1, Ordering::SeqCst);
            let task = tokio::spawn(async move {
                let _done = ReclaimTaskDone(Arc::clone(&task_inner));
                use crate::boundary::{OrderedReclaimGraphAcquire, OrderedReclaimGraphCommit};
                let _guards = (slot, lease, connection_guard, group, admission, starting);
                let mut ready = Some(ready_tx);
                let authority_request = task_inner.reclaim_authority_request.load(Ordering::SeqCst);
                let mut open_delay = std::time::Duration::from_millis(250);
                let mut open_uncertain = false;
                loop {
                    let open_gate = task_inner.reclaim_open_gate.lock().await;
                    if task_inner.reclaim_authority_retiring.load(Ordering::SeqCst)
                        || task_inner.reclaim_authority_request.load(Ordering::SeqCst)
                            != authority_request
                    {
                        if let Some(tx) = ready.take() {
                            let _ = tx.send(Err(contract_error(
                                BleErrorCode::OperationAborted,
                                BleErrorDomain::Cleanup,
                                OP,
                            )
                            .with_outcome(
                                Some(CommitState::NotDispatched),
                                Retryability::CallerDecides,
                            )));
                        }
                        return;
                    }
                    let result = tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        task_inner
                            .boundary
                            .open_ordered_reclaim_authority(authority_request),
                    )
                    .await
                    .unwrap_or_else(|_| {
                        Err(DesktopError::new(
                            BleErrorCode::OperationTimedOut,
                            BleErrorDomain::Cleanup,
                            OP,
                        ))
                    });
                    drop(open_gate);
                    match result {
                        Ok(authority) => {
                            let prior = task_inner
                                .reclaim_authority
                                .compare_exchange(0, authority, Ordering::SeqCst, Ordering::SeqCst)
                                .unwrap_or_else(|value| value);
                            if authority == 0 || (prior != 0 && prior != authority) {
                                if let Some(tx) = ready.take() {
                                    let _ = tx.send(Err(contract_error(
                                        BleErrorCode::ProtocolMalformed,
                                        BleErrorDomain::Cleanup,
                                        OP,
                                    )));
                                }
                                tokio::time::sleep(open_delay).await;
                                open_delay =
                                    (open_delay * 2).min(std::time::Duration::from_secs(30));
                                continue;
                            }
                            task_inner
                                .reclaim_authority_opened
                                .store(true, Ordering::SeqCst);
                            break;
                        }
                        Err(error) if error.commit() == Some(CommitState::NotDispatched) => {
                            if let Some(tx) = ready.take() {
                                let _ = tx.send(Err(error));
                            }
                            let open_gate = task_inner.reclaim_open_gate.lock().await;
                            if task_inner.reclaim_authority_request.load(Ordering::SeqCst)
                                != authority_request
                                || task_inner
                                    .reclaim_authority_retiring
                                    .swap(true, Ordering::SeqCst)
                            {
                                return;
                            }
                            loop {
                                if matches!(
                                    tokio::time::timeout(
                                        std::time::Duration::from_secs(5),
                                        task_inner
                                            .boundary
                                            .forget_ordered_reclaim_open(authority_request)
                                    )
                                    .await,
                                    Ok(Ok(()))
                                ) {
                                    break;
                                }
                                tokio::time::sleep(open_delay).await;
                                open_delay =
                                    (open_delay * 2).min(std::time::Duration::from_secs(30));
                            }
                            // Waiters must be allowed through the open gate to see
                            // retirement and abandon the old token without effects.
                            drop(open_gate);
                            loop {
                                let notified = task_inner.reclaim_changed.notified();
                                tokio::pin!(notified);
                                notified.as_mut().enable();
                                if task_inner.reclaim_active.load(Ordering::SeqCst) <= 1 {
                                    break;
                                }
                                notified.await;
                            }
                            task_inner.reclaim_authority.store(0, Ordering::SeqCst);
                            task_inner
                                .reclaim_authority_opened
                                .store(false, Ordering::SeqCst);
                            task_inner
                                .reclaim_authority_closed
                                .store(false, Ordering::SeqCst);
                            *lock_std(&task_inner.reclaim_ids) = ReclaimIdWindow::default();
                            if let Ok(next) = RECLAIM_AUTHORITY_COUNTER.fetch_update(
                                Ordering::SeqCst,
                                Ordering::SeqCst,
                                |id| id.checked_add(1),
                            ) {
                                task_inner
                                    .reclaim_authority_request
                                    .store(next, Ordering::SeqCst);
                                task_inner
                                    .reclaim_authority_retiring
                                    .store(false, Ordering::SeqCst);
                            }
                            return;
                        }
                        Err(error)
                            if !open_uncertain
                                && error.code() == BleErrorCode::CapabilityUnsupported =>
                        {
                            if let Some(tx) = ready.take() {
                                let _ = tx.send(Err(error));
                            }
                            return;
                        }
                        Err(_error) => {
                            // An internal probe is not the caller's deadline.
                            // Keep the same waiter pending while exact source
                            // status is reconciled by the owned coalescer.
                            open_uncertain = true;
                            tokio::time::sleep(open_delay).await;
                            open_delay = (open_delay * 2).min(std::time::Duration::from_secs(30));
                        }
                    }
                }
                // No permit ID has been issued yet. If the caller abandoned the
                // open, there is no native permit gap to retire.
                if ready.as_ref().is_none_or(|tx| tx.is_closed()) {
                    return;
                }
                let permit_request = {
                    let mut ids = lock_std(&task_inner.reclaim_ids);
                    if ids.next == u64::MAX {
                        if let Some(tx) = ready.take() {
                            let _ = tx.send(Err(contract_error(
                                BleErrorCode::LifecycleInvalidState,
                                BleErrorDomain::Cleanup,
                                OP,
                            )
                            .with_detail("reclaim permit IDs exhausted")));
                        }
                        return;
                    }
                    match ids.issue() {
                        Some(id) => id,
                        None => {
                            if let Some(tx) = ready.take() {
                                let _ = tx.send(Err(contract_error(
                                    BleErrorCode::LifecycleInvalidState,
                                    BleErrorDomain::Cleanup,
                                    OP,
                                )
                                .with_detail("reclaim permit window is full")
                                .with_outcome(
                                    Some(CommitState::NotDispatched),
                                    Retryability::CallerDecides,
                                )));
                            }
                            return;
                        }
                    }
                };
                let authority_id = task_inner.reclaim_authority.load(Ordering::SeqCst);
                let permit_request_key = crate::boundary::OrderedReclaimPermitRequest {
                    authority_id,
                    request_id: permit_request,
                    peer_id: native_peer.clone(),
                    connection_generation: connection.clone(),
                    identity: identity.clone(),
                };
                let mut reserve_delay = std::time::Duration::from_millis(250);
                let mut reserve_uncertain = false;
                let permit_id = loop {
                    let result = tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        task_inner
                            .boundary
                            .reserve_ordered_reclaim_permit(&permit_request_key),
                    )
                    .await
                    .unwrap_or_else(|_| {
                        Err(DesktopError::new(
                            BleErrorCode::OperationTimedOut,
                            BleErrorDomain::Cleanup,
                            OP,
                        ))
                    });
                    match result {
                        Ok(id) if id != 0 => break id,
                        Err(error) if error.commit() == Some(CommitState::NotDispatched) => {
                            if let Some(tx) = ready.take() {
                                let _ = tx.send(Err(error));
                            }
                            loop {
                                if matches!(
                                    tokio::time::timeout(
                                        std::time::Duration::from_secs(5),
                                        task_inner.boundary.forget_ordered_reclaim_permit_request(
                                            &permit_request_key
                                        )
                                    )
                                    .await,
                                    Ok(Ok(()))
                                ) {
                                    break;
                                }
                                tokio::time::sleep(reserve_delay).await;
                                reserve_delay =
                                    (reserve_delay * 2).min(std::time::Duration::from_secs(30));
                            }
                            lock_std(&task_inner.reclaim_ids).retire(permit_request);
                            return;
                        }
                        Err(error)
                            if !reserve_uncertain
                                && error.code() == BleErrorCode::CapabilityUnsupported =>
                        {
                            if let Some(tx) = ready.take() {
                                let _ = tx.send(Err(error));
                            }
                            lock_std(&task_inner.reclaim_ids).retire(permit_request);
                            return;
                        }
                        result => {
                            reserve_uncertain = true;
                            // Preserve the same operation while the source receipt
                            // is uncertain; do not turn an internal probe into a
                            // terminal public result.
                            let _unresolved = result;
                            tokio::time::sleep(reserve_delay).await;
                            reserve_delay =
                                (reserve_delay * 2).min(std::time::Duration::from_secs(30));
                        }
                    }
                };
                let key = crate::boundary::OrderedReclaimGraphKey {
                    authority_id,
                    peer_id: native_peer,
                    connection_generation: connection,
                    identity,
                    permit_id,
                };
                if ready.as_ref().is_none_or(|tx| tx.is_closed()) {
                    let mut delay = std::time::Duration::from_millis(250);
                    loop {
                        if matches!(
                            tokio::time::timeout(
                                std::time::Duration::from_secs(5),
                                task_inner.boundary.forget_ordered_reclaim_graph(&key)
                            )
                            .await,
                            Ok(Ok(()))
                        ) {
                            break;
                        }
                        tokio::time::sleep(delay).await;
                        delay = (delay * 2).min(std::time::Duration::from_secs(30));
                    }
                    lock_std(&task_inner.reclaim_ids).retire(permit_request);
                    return;
                }
                // An interrupted request can have reached native. Keep the exact
                // key and admission pins until its authoritative result is known.
                let mut retry_delay = std::time::Duration::from_millis(250);
                let mut acquire_uncertain = false;
                let (acquired, has_receipt) = loop {
                    let attempt = tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        task_inner.boundary.acquire_ordered_reclaim_graph(&key),
                    )
                    .await
                    .unwrap_or_else(|_| {
                        Err(DesktopError::new(
                            BleErrorCode::OperationTimedOut,
                            BleErrorDomain::Cleanup,
                            OP,
                        ))
                    });
                    match attempt {
                        Ok(OrderedReclaimGraphAcquire::Acquired) => break (true, true),
                        Ok(OrderedReclaimGraphAcquire::Stale) => {
                            if let Some(tx) = ready.take() {
                                let _ = tx.send(Err(contract_error(
                                    BleErrorCode::GattStaleHandle,
                                    BleErrorDomain::Gatt,
                                    OP,
                                )));
                            }
                            break (false, true);
                        }
                        Ok(OrderedReclaimGraphAcquire::Busy) => {
                            if let Some(tx) = ready.take() {
                                let _ = tx.send(Err(contract_error(
                                    BleErrorCode::LifecycleInvalidState,
                                    BleErrorDomain::Cleanup,
                                    OP,
                                )
                                .with_detail("native reclaim permit is busy; retry")
                                .with_outcome(
                                    Some(CommitState::NotDispatched),
                                    Retryability::CallerDecides,
                                )));
                            }
                            break (false, true);
                        }
                        Ok(OrderedReclaimGraphAcquire::NotAcquired(error)) => {
                            if let Some(tx) = ready.take() {
                                let _ = tx.send(Err(error));
                            }
                            break (false, true);
                        }
                        Err(error)
                            if !acquire_uncertain
                                && error.code() == BleErrorCode::CapabilityUnsupported =>
                        {
                            if let Some(tx) = ready.take() {
                                let _ = tx.send(Err(error));
                            }
                            break (false, true);
                        }
                        Err(_error) => {
                            acquire_uncertain = true;
                            tokio::time::sleep(retry_delay).await;
                            retry_delay = (retry_delay * 2).min(std::time::Duration::from_secs(30));
                        }
                    }
                };
                if !acquired {
                    // CapabilityUnsupported is the default no-effect boundary and
                    // does not create a native receipt. Other no-effect outcomes
                    // are retained by native until exact retirement.
                    if has_receipt {
                        let mut delay = std::time::Duration::from_millis(250);
                        loop {
                            if matches!(
                                tokio::time::timeout(
                                    std::time::Duration::from_secs(5),
                                    task_inner.boundary.forget_ordered_reclaim_graph(&key)
                                )
                                .await,
                                Ok(Ok(()))
                            ) {
                                break;
                            }
                            tokio::time::sleep(delay).await;
                            delay = (delay * 2).min(std::time::Duration::from_secs(30));
                        }
                    }
                    lock_std(&task_inner.reclaim_ids).retire(permit_request);
                    return;
                }
                let mut commit_delay = std::time::Duration::from_millis(250);
                let committed = loop {
                    if ready.as_ref().is_none_or(|tx| tx.is_closed()) {
                        break false;
                    }
                    let attempt = tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        task_inner.boundary.commit_ordered_reclaim_graph(&key),
                    )
                    .await
                    .unwrap_or_else(|_| {
                        Err(DesktopError::new(
                            BleErrorCode::OperationTimedOut,
                            BleErrorDomain::Cleanup,
                            OP,
                        ))
                    });
                    match attempt {
                        Ok(OrderedReclaimGraphCommit::Committed) => break true,
                        Ok(OrderedReclaimGraphCommit::Invalidated(error)) => {
                            if let Some(tx) = ready.take() {
                                let _ = tx.send(Err(error));
                            }
                            break false;
                        }
                        Err(error) if error.commit() == Some(CommitState::NotDispatched) => {
                            if let Some(tx) = ready.take() {
                                let _ = tx.send(Err(error));
                            }
                            break false;
                        }
                        Err(_) => {
                            tokio::time::sleep(commit_delay).await;
                            commit_delay =
                                (commit_delay * 2).min(std::time::Duration::from_secs(30));
                        }
                    }
                };
                if committed {
                    let permit = OrderedRecordingReclaimPermit {
                        release: Some(release_tx),
                        completion: completion_rx,
                    };
                    if let Some(tx) = ready.take() {
                        let _ = tx.send(Ok(permit));
                    }
                    let _ = release_rx.await;
                }
                retry_delay = std::time::Duration::from_millis(250);
                loop {
                    let attempt = tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        task_inner.boundary.release_ordered_reclaim_graph(&key),
                    )
                    .await
                    .unwrap_or_else(|_| {
                        Err(DesktopError::new(
                            BleErrorCode::OperationTimedOut,
                            BleErrorDomain::Cleanup,
                            OP,
                        ))
                    });
                    match attempt {
                        Ok(()) => break,
                        Err(error) => {
                            let _ = completion_tx.send(Some(Err(error)));
                            tokio::time::sleep(retry_delay).await;
                            retry_delay = (retry_delay * 2).min(std::time::Duration::from_secs(30));
                        }
                    }
                }
                retry_delay = std::time::Duration::from_millis(250);
                loop {
                    let attempt = tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        task_inner.boundary.forget_ordered_reclaim_graph(&key),
                    )
                    .await
                    .unwrap_or_else(|_| {
                        Err(DesktopError::new(
                            BleErrorCode::OperationTimedOut,
                            BleErrorDomain::Cleanup,
                            OP,
                        ))
                    });
                    match attempt {
                        Ok(()) => {
                            let _ = completion_tx.send(Some(Ok(())));
                            break;
                        }
                        Err(error) => {
                            let _ = completion_tx.send(Some(Err(error)));
                            tokio::time::sleep(retry_delay).await;
                            retry_delay = (retry_delay * 2).min(std::time::Duration::from_secs(30));
                        }
                    }
                }
                lock_std(&task_inner.reclaim_ids).retire(permit_request);
            });
            tasks.push(task);
        }
        ready_rx.await.unwrap_or_else(|_| {
            Err(contract_error(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Cleanup,
                OP,
            ))
        })
    }

    /// Return the one recording-owned reclaim reservation state.
    pub async fn reclaim_reservation_owner(
        &self,
        peer_id: &str,
        collection_id: u64,
        token: u64,
    ) -> Result<Arc<crate::reclaim_reservation::ReclaimReservationOwner<B>>, DesktopError> {
        const OP: &str = "gatt.ordered.reclaim-reservation";
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(BleErrorCode::OwnershipDenied, BleErrorDomain::Gatt, OP)
            })?;
        let mut state = owner.lock().await;
        super::ordered_batch_cleanup::check_recording_owner(
            &lock_std(&state.delivery_state),
            Some(token),
        )?;
        if state.receipt_acknowledged || state.closing {
            return Err(contract_error(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Gatt,
                OP,
            ));
        }
        if state
            .marker_reservation
            .as_ref()
            .is_some_and(|(held, _, _)| *held != Some(token))
        {
            return Err(contract_error(
                BleErrorCode::OwnershipDenied,
                BleErrorDomain::Gatt,
                OP,
            ));
        }
        let (_, _, gate) = state.marker_reservation.get_or_insert_with(|| {
            let reservation = crate::marker_reservation::MarkerReservationState::new();
            let gate = Arc::clone(&reservation.gate);
            (Some(token), Arc::new(Mutex::new(reservation)), gate)
        });
        let gate = Arc::clone(gate);
        let shared = Arc::clone(state.reclaim_reservation.get_or_insert_with(|| {
            Arc::new(StdMutex::new(
                crate::reclaim_reservation::ReclaimReservationState::new(),
            ))
        }));
        Ok(Arc::new(
            crate::reclaim_reservation::ReclaimReservationOwner::new(
                self.clone(),
                peer_id.to_owned(),
                collection_id,
                token,
                shared,
                gate,
            ),
        ))
    }

    /// Return the one group-owned reservation authority after checking its
    /// exact normal-session or recorder capability. Clones share retained state.
    pub async fn marker_reservation_owner(
        &self,
        peer_id: &str,
        collection_id: u64,
        token: Option<u64>,
    ) -> Result<Arc<crate::marker_reservation::MarkerReservationOwner<B>>, DesktopError> {
        const OP: &str = "gatt.ordered.marker-reservation";
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(BleErrorCode::OwnershipDenied, BleErrorDomain::Gatt, OP)
            })?;
        let mut state = owner.lock().await;
        super::ordered_batch_cleanup::check_recording_owner(
            &lock_std(&state.delivery_state),
            token,
        )?;
        if state.receipt_acknowledged {
            return Err(contract_error(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Gatt,
                OP,
            ));
        }
        if state
            .marker_reservation
            .as_ref()
            .is_some_and(|(held, _, _)| *held != token)
        {
            return Err(contract_error(
                BleErrorCode::OwnershipDenied,
                BleErrorDomain::Gatt,
                OP,
            ));
        }
        let (_, shared, _) = state.marker_reservation.get_or_insert_with(|| {
            let reservation = crate::marker_reservation::MarkerReservationState::new();
            let gate = Arc::clone(&reservation.gate);
            (token, Arc::new(Mutex::new(reservation)), gate)
        });
        Ok(Arc::new(
            crate::marker_reservation::MarkerReservationOwner::new_with_state(
                self.clone(),
                peer_id.to_owned(),
                collection_id,
                token,
                Arc::clone(shared),
            ),
        ))
    }

    /// Coalesced wake for atomic marker transaction publication. Register and
    /// enable a waiter before checking `ordered_marker_next_id`.
    pub async fn ordered_marker_publication_wake(
        &self,
        peer_id: &str,
        collection_id: u64,
        token: Option<u64>,
    ) -> Result<Arc<tokio::sync::Notify>, DesktopError> {
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.marker-publication",
                )
            })?;
        let state = owner.lock().await;
        super::ordered_batch_cleanup::check_recording_owner(
            &lock_std(&state.delivery_state),
            token,
        )?;
        Ok(Arc::clone(&state.marker_publication_wake))
    }

    /// Return the current checked per-group ID and whether it already has an
    /// exact retained transaction. Preparing a selector consumes no ID.
    pub async fn ordered_marker_next_id(
        &self,
        peer_id: &str,
        collection_id: u64,
        token: Option<u64>,
    ) -> Result<(Option<u64>, bool), DesktopError> {
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.marker-id",
                )
            })?;
        let state = owner.lock().await;
        super::ordered_batch_cleanup::check_recording_owner(
            &lock_std(&state.delivery_state),
            token,
        )?;
        Ok((state.next_marker_id, state.marker_transaction.is_some()))
    }

    fn checked_marker_target(
        &self,
        core: &Central,
        peer_key: &str,
        peer_id: &str,
        selector: &PathSelector,
        with_response: bool,
    ) -> Result<InstanceKey, DesktopError> {
        const OP: &str = "gatt.ordered.marker-prepare";
        if selector.characteristic_uuid.is_none()
            || selector.descriptor_uuid.is_some()
            || selector.descriptor_occurrence.is_some()
        {
            return Err(contract_error(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Gatt,
                OP,
            ));
        }
        let (index, target, _) =
            self.resolve_instance(core, peer_key, peer_id, selector, OP, false)?;
        let path = core.stored_path(index).ok_or_else(|| {
            contract_error(BleErrorCode::GattStaleHandle, BleErrorDomain::Gatt, OP)
        })?;
        let required = if with_response {
            ubm_core::central::GATT_PROP_WRITE
        } else {
            ubm_core::central::GATT_PROP_WRITE_NO_RESPONSE
        };
        if path.properties() & required == 0 {
            return Err(contract_error(
                BleErrorCode::GattPropertyNotSupported,
                BleErrorDomain::Gatt,
                OP,
            ));
        }
        self.inner
            .acquired
            .assert_available(&target, crate::acquired_gatt::AcquisitionKind::Write)?;
        Ok(target)
    }

    /// Resolve a write target and bind it to the group's captured graph. The
    /// returned exact request is reused for status and idempotent retry.
    pub async fn prepare_ordered_marker_request(
        &self,
        peer_id: &str,
        collection_id: u64,
        token: Option<u64>,
        marker_id: u64,
        selector: &PathSelector,
        value: Vec<u8>,
        with_response: bool,
    ) -> Result<OrderedMarkerWriteRequest, DesktopError> {
        const OP: &str = "gatt.ordered.marker-prepare";
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(BleErrorCode::OwnershipDenied, BleErrorDomain::Gatt, OP)
            })?;
        let (connection, database, snapshot) = {
            let state = owner.lock().await;
            super::ordered_batch_cleanup::check_recording_owner(
                &lock_std(&state.delivery_state),
                token,
            )?;
            if state.closing || state.retired {
                return Err(contract_error(
                    BleErrorCode::GattStaleHandle,
                    BleErrorDomain::Gatt,
                    OP,
                ));
            }
            (
                state.connection_generation.clone(),
                state.database_generation.clone(),
                state.snapshot_identity.clone(),
            )
        };
        let connection = connection.ok_or_else(|| {
            contract_error(BleErrorCode::GattStaleHandle, BleErrorDomain::Gatt, OP)
        })?;
        let database = database.ok_or_else(|| {
            contract_error(BleErrorCode::GattStaleHandle, BleErrorDomain::Gatt, OP)
        })?;
        let snapshot_identity = snapshot.ok_or_else(|| {
            contract_error(
                BleErrorCode::GattDiscoveryRequired,
                BleErrorDomain::Gatt,
                OP,
            )
        })?;
        let peer_key = self.known_peer_key(peer_id).await?;
        let core = self.inner.core.lock().await;
        if core.connection_generation(&peer_key).as_deref() != Some(&connection)
            || core.database_generation(&peer_key).as_deref() != Some(&database)
        {
            return Err(contract_error(
                BleErrorCode::GattStaleHandle,
                BleErrorDomain::Gatt,
                OP,
            ));
        }
        let target =
            self.checked_marker_target(&core, &peer_key, peer_id, selector, with_response)?;
        Ok(OrderedMarkerWriteRequest {
            marker_id,
            connection_generation: connection,
            snapshot_identity,
            target,
            value,
            with_response,
        })
    }

    /// Start once or replay the exact group transaction. The worker remains
    /// owned after caller cancellation; the native owner retains effects.
    pub async fn start_ordered_marker_write(
        &self,
        peer_id: &str,
        collection_id: u64,
        recording_token: Option<u64>,
        request: OrderedMarkerWriteRequest,
    ) -> Result<OrderedMarkerWriteStatus, DesktopError> {
        self.start_ordered_marker_write_with_intent(
            peer_id,
            collection_id,
            recording_token,
            request,
            Arc::new(std::sync::atomic::AtomicU8::new(0)),
        )
        .await
    }

    pub(crate) async fn start_ordered_marker_write_with_intent(
        &self,
        peer_id: &str,
        collection_id: u64,
        recording_token: Option<u64>,
        request: OrderedMarkerWriteRequest,
        intent: Arc<std::sync::atomic::AtomicU8>,
    ) -> Result<OrderedMarkerWriteStatus, DesktopError> {
        let central = self.clone();
        let peer = peer_id.to_owned();
        let (send, receive) = tokio::sync::oneshot::channel();
        let inner = Arc::clone(&self.inner);
        spawn_retirement(&inner, async move {
            let result = central
                .start_ordered_marker_write_owned(
                    &peer,
                    collection_id,
                    recording_token,
                    request,
                    intent,
                )
                .await;
            let _ = send.send(result);
        });
        receive.await.map_err(|_| {
            contract_error(
                BleErrorCode::OperationReset,
                BleErrorDomain::Gatt,
                "gatt.ordered.marker-write",
            )
        })?
    }

    async fn start_ordered_marker_write_owned(
        &self,
        peer_id: &str,
        collection_id: u64,
        token: Option<u64>,
        request: OrderedMarkerWriteRequest,
        intent: Arc<std::sync::atomic::AtomicU8>,
    ) -> Result<OrderedMarkerWriteStatus, DesktopError> {
        // BLE attribute writes are bounded before reserving a marker or native
        // transaction ID. Include the exact identity in the retention bound.
        let identity_bytes = request
            .target
            .0
            .len()
            .saturating_add(request.target.1.len())
            .saturating_add(request.target.3.len())
            .saturating_add(request.connection_generation.len())
            .saturating_add(request.snapshot_identity.owner.len())
            .saturating_add(17);
        if request.target.2 > i64::MAX as u64 || request.target.4 > i64::MAX as u64 {
            return Err(contract_error(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Gatt,
                "gatt.ordered.marker-write",
            )
            .with_detail("marker target occurrence exceeds the native selector range"));
        }
        if request.value.len() > crate::boundary::ATT_MAX_ATTRIBUTE_VALUE as usize
            || identity_bytes.saturating_add(request.value.len())
                > ubm_core::contracts::MAX_OPERATION_BYTES as usize
        {
            return Err(contract_error(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Gatt,
                "gatt.ordered.marker-write",
            )
            .with_detail("marker/write request exceeds its bounded retention limit"));
        }
        if !self.inner.boundary.ordered_marker_write_supported() {
            return Err(contract_error(
                BleErrorCode::CapabilityLimited,
                BleErrorDomain::Gatt,
                "gatt.ordered.marker-write",
            )
            .with_detail("marker/write transaction is unavailable"));
        }
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.marker-write",
                )
            })?;
        let gate = Arc::clone(&owner.lock().await.marker_gate);
        let _marker_gate = gate.lock().await;
        let (connection, database) = {
            let state = owner.lock().await;
            super::ordered_batch_cleanup::check_recording_owner(
                &lock_std(&state.delivery_state),
                token,
            )?;
            if let Some(existing) = &state.marker_transaction {
                let existing = lock_std(existing);
                if existing.request != request || existing.owner != token {
                    return Err(contract_error(
                        BleErrorCode::OwnershipDenied,
                        BleErrorDomain::Gatt,
                        "gatt.ordered.marker-write",
                    ));
                }
                return Ok(existing.status.clone());
            }
            (
                state.connection_generation.clone(),
                state.database_generation.clone(),
            )
        };
        let peer_key = self.known_peer_key(peer_id).await?;
        {
            let core = self.inner.core.lock().await;
            if core.connection_generation(&peer_key) != connection
                || core.database_generation(&peer_key) != database
            {
                return Err(contract_error(
                    BleErrorCode::GattStaleHandle,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.marker-write",
                ));
            }
            let selector = PathSelector {
                service_uuid: request.target.1.clone(),
                service_occurrence: Some(request.target.2),
                characteristic_uuid: Some(request.target.3.clone()),
                characteristic_occurrence: Some(request.target.4),
                descriptor_uuid: None,
                descriptor_occurrence: None,
            };
            if self.checked_marker_target(
                &core,
                &peer_key,
                peer_id,
                &selector,
                request.with_response,
            )? != request.target
            {
                return Err(contract_error(
                    BleErrorCode::GattStaleHandle,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.marker-write",
                ));
            }
        }
        #[cfg(test)]
        {
            let seam = lock_std(&self.inner.marker_before_publication).take();
            if let Some((entered, release)) = seam {
                entered.notify_one();
                release.notified().await;
            }
        }
        // Hold delivery admission and the batch owner across both credit
        // reservation and publication: lifecycle cannot retire the source in
        // the gap and cancellation cannot strand a credit without its owner.
        let delivery_gate = Arc::clone(&owner.lock().await.delivery_lock);
        let _delivery_gate = delivery_gate.lock().await;
        let transaction = {
            let mut state = owner.lock().await;
            {
                let delivery = lock_std(&state.delivery_state);
                super::ordered_batch_cleanup::check_recording_owner(&delivery, token)?;
                if delivery.marker_owner.is_some() {
                    if let Some(existing) = &state.marker_transaction {
                        let existing = lock_std(existing);
                        if existing.request == request && existing.owner == token {
                            return Ok(existing.status.clone());
                        }
                    }
                    return Err(contract_error(
                        BleErrorCode::OwnershipDenied,
                        BleErrorDomain::Gatt,
                        "gatt.ordered.marker-write",
                    ));
                }
            }
            if let Some(existing) = &state.marker_transaction {
                let existing = lock_std(existing);
                if existing.request != request || existing.owner != token {
                    return Err(contract_error(
                        BleErrorCode::OwnershipDenied,
                        BleErrorDomain::Gatt,
                        "gatt.ordered.marker-write",
                    ));
                }
                return Ok(existing.status.clone());
            }
            if state.next_marker_id != Some(request.marker_id)
                || state.connection_generation.as_deref() != Some(&request.connection_generation)
                || state.snapshot_identity.as_ref() != Some(&request.snapshot_identity)
                || request.target.0 != peer_id
                || state.closing
                || state.retired
            {
                return Err(contract_error(
                    BleErrorCode::GattStaleHandle,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.marker-write",
                ));
            }
            let lease = match &state.source {
                Some(OrderedSourceCleanup::Lease(lease)) => lease.clone(),
                _ => {
                    return Err(contract_error(
                        BleErrorCode::LifecycleInvalidState,
                        BleErrorDomain::Gatt,
                        "gatt.ordered.marker-write",
                    ));
                }
            };
            match lease.ingress {
                crate::boundary::OrderedSourceIngress::BackendOwned => self
                    .inner
                    .boundary
                    .reserve_ordered_marker(&lease, request.marker_id)?,
                crate::boundary::OrderedSourceIngress::CentralRetainedBatch => {
                    lock_std(&self.inner.mobile_ordered_ingress)
                        .reserve_marker(
                            collection_id,
                            &super::ordered_claims::ordered_source_key(&lease),
                            request.marker_id,
                        )
                        .map_err(|error| {
                            contract_error(
                                BleErrorCode::CapabilityLimited,
                                BleErrorDomain::Gatt,
                                "gatt.ordered.marker-credit",
                            )
                            .with_detail(format!("{error:?}"))
                        })?
                }
            }
            lock_std(&state.delivery_state).marker_owner = Some((request.marker_id, token));
            let transaction = Arc::new(StdMutex::new(MarkerTransaction {
                request: request.clone(),
                lease,
                owner: token,
                status: OrderedMarkerWriteStatus::Pending,
                error: None,
                intent: Arc::clone(&intent),
            }));
            state.marker_transaction = Some(Arc::clone(&transaction));
            state.marker_publication_wake.notify_waiters();
            transaction
        };
        drop(_delivery_gate);
        // Native start is independently owned; status must remain queryable
        // while that operation is pending.
        drop(_marker_gate);
        let lease = lock_std(&transaction).lease.clone();
        const OP: &str = "gatt.ordered.marker-write";
        let ctl = OpControl::unbounded();
        let window = ctl.budget.window(LIVENESS_OP);
        let admission = self.wait_gatt_admission(peer_id, &ctl, OP, window).await;
        let answer = match admission {
            Err(error) => {
                self.refuse_marker_before_dispatch(&lease, &request, &transaction, error)
                    .await
            }
            Ok(admission) => {
                let starting = match drive_link(
                    &self.inner,
                    peer_id,
                    OP,
                    &ctl.ticket,
                    window,
                    admission.begin_starting(),
                )
                .await
                {
                    Wait::Done(result) => result,
                    Wait::Expired => Err(timed_out(OP, window)),
                    Wait::Cancelled => Err(ctl.ticket.interruption(OP)),
                };
                match starting {
                    Err(error) => {
                        drop(admission);
                        self.refuse_marker_before_dispatch(&lease, &request, &transaction, error)
                            .await
                    }
                    Ok(starting) => {
                        let validation = {
                            let core = self.inner.core.lock().await;
                            if core.connection_generation(&peer_key) != connection
                                || core.database_generation(&peer_key) != database
                            {
                                Err(contract_error(
                                    BleErrorCode::GattStaleHandle,
                                    BleErrorDomain::Gatt,
                                    OP,
                                ))
                            } else {
                                let selector = PathSelector {
                                    service_uuid: request.target.1.clone(),
                                    service_occurrence: Some(request.target.2),
                                    characteristic_uuid: Some(request.target.3.clone()),
                                    characteristic_occurrence: Some(request.target.4),
                                    descriptor_uuid: None,
                                    descriptor_occurrence: None,
                                };
                                self.checked_marker_target(
                                    &core,
                                    &peer_key,
                                    peer_id,
                                    &selector,
                                    request.with_response,
                                )
                                .and_then(|target| {
                                    if target == request.target {
                                        Ok(())
                                    } else {
                                        Err(contract_error(
                                            BleErrorCode::GattStaleHandle,
                                            BleErrorDomain::Gatt,
                                            OP,
                                        ))
                                    }
                                })
                            }
                        };
                        match validation {
                            Err(error) => {
                                drop(starting);
                                drop(admission);
                                self.refuse_marker_before_dispatch(
                                    &lease,
                                    &request,
                                    &transaction,
                                    error,
                                )
                                .await
                            }
                            Ok(()) => {
                                // Cancellation/forget and the native first poll share
                                // this gate: a retired NotAdmitted request can never
                                // be recreated by a later queued start.
                                let first_gate = gate.lock().await;
                                let prior = lock_std(&transaction).status.clone();
                                if matches!(prior, OrderedMarkerWriteStatus::NotAdmitted(_)) {
                                    drop(first_gate);
                                    drop(starting);
                                    drop(admission);
                                    Ok(prior)
                                } else if intent
                                    .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire)
                                    .is_err()
                                {
                                    drop(first_gate);
                                    drop(starting);
                                    drop(admission);
                                    self.refuse_marker_before_dispatch(
                                        &lease,
                                        &request,
                                        &transaction,
                                        DesktopError::cancelled(OP),
                                    )
                                    .await
                                } else {
                                    let mut first_gate = Some(first_gate);
                                    let mut starting = Some(starting);
                                    let native = self
                                        .inner
                                        .boundary
                                        .start_ordered_marker_write(&lease, &request);
                                    tokio::pin!(native);
                                    std::future::poll_fn(|context| {
                                        let result = native.as_mut().poll(context);
                                        if starting.is_some() {
                                            admission.mark_dispatched();
                                            starting.take();
                                            first_gate.take();
                                        }
                                        result
                                    })
                                    .await
                                }
                            }
                        }
                    }
                }
            }
        };
        let _marker_gate = gate.lock().await;
        match answer {
            Ok(status) => {
                let apply = {
                    let mut tx = lock_std(&transaction);
                    if matches!(tx.status, OrderedMarkerWriteStatus::Pending) {
                        tx.status = status.clone();
                        true
                    } else {
                        false
                    }
                };
                if apply && matches!(status, OrderedMarkerWriteStatus::NotAdmitted(_)) {
                    self.release_ordered_marker_credit(
                        peer_id,
                        collection_id,
                        request.marker_id,
                        token,
                    )
                    .await?;
                }
                let current = lock_std(&transaction).status.clone();
                Ok(current)
            }
            Err(error) => {
                lock_std(&transaction).error = Some(error.clone());
                Err(error)
            }
        }
    }

    async fn refuse_marker_before_dispatch(
        &self,
        lease: &OrderedSourceLease,
        request: &OrderedMarkerWriteRequest,
        transaction: &Arc<StdMutex<MarkerTransaction>>,
        error: DesktopError,
    ) -> Result<OrderedMarkerWriteStatus, DesktopError> {
        lock_std(transaction).error = Some(error.clone());
        match self
            .inner
            .boundary
            .cancel_ordered_marker_write(lease, request)
            .await?
        {
            OrderedMarkerWriteStatus::NotAdmitted(_) => {
                Ok(OrderedMarkerWriteStatus::NotAdmitted(Some(error)))
            }
            other => Ok(other),
        }
    }

    /// Recover the immutable transaction identity without consulting the
    /// current graph, including after service change or disconnect.
    pub async fn retained_ordered_marker_request(
        &self,
        peer_id: &str,
        collection_id: u64,
        token: Option<u64>,
        marker_id: u64,
    ) -> Result<OrderedMarkerWriteRequest, DesktopError> {
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.marker-request",
                )
            })?;
        let state = owner.lock().await;
        super::ordered_batch_cleanup::check_recording_owner(
            &lock_std(&state.delivery_state),
            token,
        )?;
        let transaction = state.marker_transaction.as_ref().ok_or_else(|| {
            contract_error(
                BleErrorCode::OwnershipDenied,
                BleErrorDomain::Gatt,
                "gatt.ordered.marker-request",
            )
        })?;
        let transaction = lock_std(transaction);
        if transaction.owner != token || transaction.request.marker_id != marker_id {
            return Err(contract_error(
                BleErrorCode::OwnershipDenied,
                BleErrorDomain::Gatt,
                "gatt.ordered.marker-request",
            ));
        }
        Ok(transaction.request.clone())
    }

    /// Query a retained marker by ID without resolving a replacement graph.
    pub async fn ordered_marker_write_status_by_id(
        &self,
        peer_id: &str,
        collection_id: u64,
        token: Option<u64>,
        marker_id: u64,
    ) -> Result<OrderedMarkerWriteStatus, DesktopError> {
        let request = self
            .retained_ordered_marker_request(peer_id, collection_id, token, marker_id)
            .await?;
        self.ordered_marker_write_status(peer_id, collection_id, token, &request)
            .await
    }

    /// Forget a terminal marker; already-retired issued IDs are idempotent.
    pub async fn forget_ordered_marker_write_by_id(
        &self,
        peer_id: &str,
        collection_id: u64,
        token: Option<u64>,
        marker_id: u64,
    ) -> Result<(), DesktopError> {
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.marker-write-forget",
                )
            })?;
        {
            let state = owner.lock().await;
            super::ordered_batch_cleanup::check_recording_owner(
                &lock_std(&state.delivery_state),
                token,
            )?;
            if state
                .retired_marker_high_water
                .is_some_and(|last| marker_id <= last)
            {
                return Ok(());
            }
        }
        let request = self
            .retained_ordered_marker_request(peer_id, collection_id, token, marker_id)
            .await?;
        self.forget_ordered_marker_write(peer_id, collection_id, token, &request)
            .await
    }

    /// Request cancellation of the exact native transaction. The owned worker
    /// completes even if its caller disappears, and never reverses admission.
    pub async fn cancel_ordered_marker_write_by_id(
        &self,
        peer_id: &str,
        collection_id: u64,
        token: Option<u64>,
        marker_id: u64,
    ) -> Result<OrderedMarkerWriteStatus, DesktopError> {
        let central = self.clone();
        let peer = peer_id.to_owned();
        let (send, receive) = tokio::sync::oneshot::channel();
        let inner = Arc::clone(&self.inner);
        spawn_retirement(&inner, async move {
            let result = central
                .cancel_ordered_marker_write_owned(&peer, collection_id, token, marker_id)
                .await;
            let _ = send.send(result);
        });
        receive.await.map_err(|_| {
            contract_error(
                BleErrorCode::OperationReset,
                BleErrorDomain::Gatt,
                "gatt.ordered.marker-write-cancel",
            )
        })?
    }

    async fn cancel_ordered_marker_write_owned(
        &self,
        peer_id: &str,
        collection_id: u64,
        token: Option<u64>,
        marker_id: u64,
    ) -> Result<OrderedMarkerWriteStatus, DesktopError> {
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.marker-write-cancel",
                )
            })?;
        let gate = Arc::clone(&owner.lock().await.marker_gate);
        let _marker_gate = gate.lock().await;
        let (transaction, lease, request) = {
            let state = owner.lock().await;
            super::ordered_batch_cleanup::check_recording_owner(
                &lock_std(&state.delivery_state),
                token,
            )?;
            let transaction = state.marker_transaction.clone().ok_or_else(|| {
                contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.marker-write-cancel",
                )
            })?;
            let (lease, request) = {
                let tx = lock_std(&transaction);
                if tx.owner != token || tx.request.marker_id != marker_id {
                    return Err(contract_error(
                        BleErrorCode::OwnershipDenied,
                        BleErrorDomain::Gatt,
                        "gatt.ordered.marker-write-cancel",
                    ));
                }
                let _ = tx
                    .intent
                    .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire);
                (tx.lease.clone(), tx.request.clone())
            };
            (transaction, lease, request)
        };
        let native_status = self
            .inner
            .boundary
            .cancel_ordered_marker_write(&lease, &request)
            .await?;
        let status = {
            let tx = lock_std(&transaction);
            match (native_status, tx.error.clone()) {
                (OrderedMarkerWriteStatus::NotAdmitted(_), Some(error)) => {
                    OrderedMarkerWriteStatus::NotAdmitted(Some(error))
                }
                (other, _) => other,
            }
        };
        lock_std(&transaction).status = status.clone();
        if matches!(status, OrderedMarkerWriteStatus::NotAdmitted(_)) {
            let pending = {
                let state = owner.lock().await;
                lock_std(&state.delivery_state).marker_owner == Some((marker_id, token))
            };
            if pending {
                self.release_ordered_marker_credit(peer_id, collection_id, marker_id, token)
                    .await?;
            }
        }
        Ok(status)
    }

    /// Query native ownership; Pending/Started never release downstream credit.
    pub async fn ordered_marker_write_status(
        &self,
        peer_id: &str,
        collection_id: u64,
        token: Option<u64>,
        request: &OrderedMarkerWriteRequest,
    ) -> Result<OrderedMarkerWriteStatus, DesktopError> {
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.marker-write-status",
                )
            })?;
        let gate = Arc::clone(&owner.lock().await.marker_gate);
        let _marker_gate = gate.lock().await;
        let transaction = owner
            .lock()
            .await
            .marker_transaction
            .clone()
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.marker-write-status",
                )
            })?;
        let lease = {
            let tx = lock_std(&transaction);
            if tx.owner != token || &tx.request != request {
                return Err(contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.marker-write-status",
                ));
            }
            tx.lease.clone()
        };
        let native_status = self
            .inner
            .boundary
            .ordered_marker_write_status(&lease, request)
            .await?;
        let status = {
            let tx = lock_std(&transaction);
            match (native_status, tx.error.clone()) {
                (OrderedMarkerWriteStatus::NotAdmitted(_), Some(error)) => {
                    OrderedMarkerWriteStatus::NotAdmitted(Some(error))
                }
                (other, _) => other,
            }
        };
        lock_std(&transaction).status = status.clone();
        if matches!(status, OrderedMarkerWriteStatus::NotAdmitted(_)) {
            let pending = {
                let state = owner.lock().await;
                lock_std(&state.delivery_state).marker_owner == Some((request.marker_id, token))
            };
            if pending {
                self.release_ordered_marker_credit(
                    peer_id,
                    collection_id,
                    request.marker_id,
                    token,
                )
                .await?;
            }
        }
        Ok(status)
    }

    /// Forget only after final native status and exact FIFO evidence are ACKed.
    pub async fn forget_ordered_marker_write(
        &self,
        peer_id: &str,
        collection_id: u64,
        token: Option<u64>,
        request: &OrderedMarkerWriteRequest,
    ) -> Result<(), DesktopError> {
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.marker-write-forget",
                )
            })?;
        let gate = Arc::clone(&owner.lock().await.marker_gate);
        let _marker_gate = gate.lock().await;
        let state = owner.lock().await;
        super::ordered_batch_cleanup::check_recording_owner(
            &lock_std(&state.delivery_state),
            token,
        )?;
        let Some(transaction) = state.marker_transaction.clone() else {
            return if state
                .retired_marker_high_water
                .is_some_and(|last| request.marker_id <= last)
            {
                Ok(())
            } else {
                Err(contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.marker-write-forget",
                ))
            };
        };
        let (lease, status) = {
            let tx = lock_std(&transaction);
            if tx.owner != token || tx.request != *request {
                return Err(contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.marker-write-forget",
                ));
            }
            (tx.lease.clone(), tx.status.clone())
        };
        let requires_marker = match status {
            OrderedMarkerWriteStatus::NotAdmitted(_) => false,
            OrderedMarkerWriteStatus::Rejected(_) => true,
            OrderedMarkerWriteStatus::Admitted {
                write: OrderedMarkerWritePhase::NotStarted(_) | OrderedMarkerWritePhase::Settled(_),
                ..
            } => true,
            _ => {
                return Err(contract_error(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.marker-write-forget",
                ));
            }
        };
        if requires_marker
            && lock_std(&state.delivery_state).marker_evidence_acked != Some(request.marker_id)
        {
            return Err(contract_error(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Gatt,
                "gatt.ordered.marker-write-forget",
            )
            .with_detail("marker evidence remains unacknowledged"));
        }
        let credit_pending =
            lock_std(&state.delivery_state).marker_owner == Some((request.marker_id, token));
        drop(state);
        // An earlier authoritative NotAdmitted result may have been stored
        // even if releasing its reservation failed. Retain the transaction
        // until that exact cleanup succeeds.
        if !requires_marker && credit_pending {
            self.release_ordered_marker_credit(peer_id, collection_id, request.marker_id, token)
                .await?;
        }
        self.inner
            .boundary
            .forget_ordered_marker_write(&lease, request)
            .await?;
        if requires_marker {
            self.finish_ordered_marker_credit(peer_id, collection_id, request.marker_id, token)
                .await?;
        }
        let mut state = owner.lock().await;
        state.marker_transaction = None;
        state.retired_marker_high_water = Some(request.marker_id);
        state.next_marker_id = request.marker_id.checked_add(1);
        Ok(())
    }
}
