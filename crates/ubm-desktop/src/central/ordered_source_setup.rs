// crates/ubm-desktop/src/central/ordered_source_setup.rs
//! Preparation and sequential enabling for a retained ordered batch.

use super::ordered_claims::{OrderedClaimPhase, ordered_source_key};
/// Keeps lifecycle from declaring Open absent while its setup driver can poll.
struct SourceOpenDriver<B: RadioBoundary> {
    central: DesktopCentral<B>,
    owner: Arc<Mutex<super::ordered_claims::OrderedBatchState>>,
}

impl<B: RadioBoundary> Drop for SourceOpenDriver<B> {
    fn drop(&mut self) {
        let central = self.central.clone();
        let owner = Arc::clone(&self.owner);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let mut state = owner.lock().await;
                state.source_open_driver_active = false;
                state.data_ready.notify_one();
                drop(state);
                let _ = central.inner.native_wake.send(());
            });
        }
    }
}

use super::*;

impl<B: RadioBoundary> DesktopCentral<B> {
    /// Prepare native targets and atomically install the source route before
    /// any group-owned CCCD enable. Exact generations are revalidated across
    /// each asynchronous boundary; failure leaves this ledger inspectable.
    pub(super) async fn install_ordered_batch_source(
        &self,
        peer_id: &str,
        collection_id: u64,
        max_items: usize,
        max_bytes: usize,
        max_controls: usize,
        ctl: OpControl,
    ) -> Result<crate::boundary::OrderedSourceLease, DesktopError> {
        let result = self
            .install_ordered_batch_source_inner(
                peer_id,
                collection_id,
                max_items,
                max_bytes,
                max_controls,
                ctl,
            )
            .await;
        if let Err(error) = &result {
            if let Some(owner) = self.inner.ordered_batches.get(peer_id, collection_id) {
                let mut state = owner.lock().await;
                if state.setup_error.is_none() {
                    state.setup_error = Some(error.clone());
                }
            }
        }
        result
    }

    async fn install_ordered_batch_source_inner(
        &self,
        peer_id: &str,
        collection_id: u64,
        max_items: usize,
        max_bytes: usize,
        max_controls: usize,
        ctl: OpControl,
    ) -> Result<crate::boundary::OrderedSourceLease, DesktopError> {
        use crate::boundary::{OrderedSourceCleanup, OrderedSourceMember, OrderedSourceRequest};
        use ubm_core::ordered_notifications::DeliveryMode as OrderedDelivery;
        const OP: &str = "gatt.ordered.source";
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(BleErrorCode::OwnershipDenied, BleErrorDomain::Gatt, OP)
            })?;
        let mut state = owner.lock().await;
        let peer_key = self.known_peer_key(peer_id).await?;
        let connection_generation = state.connection_generation.clone().ok_or_else(|| {
            contract_error(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Gatt,
                OP,
            )
        })?;
        let database_generation = state.database_generation.clone().ok_or_else(|| {
            contract_error(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Gatt,
                OP,
            )
        })?;
        let window = ctl.budget.window(LIVENESS_OP);
        let mut members = Vec::with_capacity(state.claims.len());
        for claim in &mut state.claims {
            {
                let core = self.inner.core.lock().await;
                if core.connection_generation(&peer_key).as_deref() != Some(&connection_generation)
                    || core.database_generation(&peer_key).as_deref() != Some(&database_generation)
                {
                    return Err(contract_error(
                        BleErrorCode::GattStaleHandle,
                        BleErrorDomain::Gatt,
                        OP,
                    ));
                }
            }
            if claim.phase != OrderedClaimPhase::Admitted {
                return Err(contract_error(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Gatt,
                    OP,
                ));
            }
            claim.phase = OrderedClaimPhase::Preparing;
            let prepared = match drive_link(
                &self.inner,
                peer_id,
                OP,
                &ctl.ticket,
                window,
                self.inner.boundary.prepare_notification_activation(
                    &claim.scope,
                    claim.epoch,
                    claim.delivery,
                    claim.preference,
                ),
            )
            .await
            {
                Wait::Done(result) => result?,
                Wait::Expired => return Err(timed_out(OP, window)),
                Wait::Cancelled => return Err(ctl.ticket.interruption(OP)),
            };
            claim.activation = Some(prepared.clone());
            claim.phase = OrderedClaimPhase::Prepared;
            let delivery = match claim.delivery {
                Some(DeliveryMode::Notification) => OrderedDelivery::Notification,
                Some(DeliveryMode::Indication) => OrderedDelivery::Indication,
                None => OrderedDelivery::Either,
            };
            members.push(OrderedSourceMember {
                member_id: claim.member_id,
                activation: prepared,
                delivery,
            });
        }
        {
            let core = self.inner.core.lock().await;
            if core.connection_generation(&peer_key).as_deref() != Some(&connection_generation)
                || core.database_generation(&peer_key).as_deref() != Some(&database_generation)
            {
                return Err(contract_error(
                    BleErrorCode::GattStaleHandle,
                    BleErrorDomain::Gatt,
                    OP,
                ));
            }
        }
        state.source_prepare_started = true;
        state.source_open_driver_active = true;
        state.source_open_first_polled = false;
        state.source_limits = Some((max_items, max_bytes, max_controls));
        state.source = Some(OrderedSourceCleanup::Indeterminate {
            peer_id: peer_id.to_owned(),
            collection_id,
        });
        let request = OrderedSourceRequest {
            snapshot_identity: state.snapshot_identity.clone(),
            peer_id: peer_id.to_owned(),
            connection_generation: connection_generation.clone(),
            database_generation: database_generation.clone(),
            collection_id,
            members,
            max_items,
            max_bytes,
            max_controls,
        };
        // Keep the ledger lock through the first native poll. Lifecycle can
        // only classify NotStarted after it knows Open cannot start later.
        // Release the lock immediately after that poll so it can adopt a
        // pending or late completion without waiting for native Open.
        let _driver = SourceOpenDriver {
            central: self.clone(),
            owner: Arc::clone(&owner),
        };
        #[cfg(test)]
        {
            let seam = lock_std(&self.inner.ordered_before_open).take();
            if let Some((entered, release)) = seam {
                entered.notify_one();
                tokio::select! {
                    biased;
                    () = ctl.ticket.cancelled() => return Err(ctl.ticket.interruption(OP)),
                    () = release.notified() => {}
                }
            }
        }
        let native = self.inner.boundary.prepare_ordered_source(request);
        let mut native = std::pin::pin!(native);
        let mut first_poll_owner = Some(state);
        let dispatch = std::future::poll_fn(move |context| {
            if let Some(owner) = first_poll_owner.as_mut() {
                owner.source_open_first_polled = true;
            }
            let result = native.as_mut().poll(context);
            if let Some(owner) = first_poll_owner.as_mut() {
                owner.starting.take();
            }
            first_poll_owner.take();
            result.map(Ok)
        });
        let answer = drive_link(&self.inner, peer_id, OP, &ctl.ticket, window, dispatch).await;
        let mut state = owner.lock().await;
        state.source_open_driver_active = false;
        state.data_ready.notify_one();
        let _ = self.inner.native_wake.send(());
        let lease = match answer {
            Wait::Done(Ok(Ok(lease))) => lease,
            Wait::Done(Ok(Err(failure))) => {
                state.source_prepare_started = failure.cleanup.is_some();
                if let Some(OrderedSourceCleanup::Lease(lease)) = &failure.cleanup {
                    state.observed_source_generation = Some(lease.source_generation.clone());
                }
                state.source = failure.cleanup;
                return Err(failure.error);
            }
            Wait::Done(Err(error)) => return Err(error),
            Wait::Expired => return Err(timed_out(OP, window)),
            Wait::Cancelled => return Err(ctl.ticket.interruption(OP)),
        };
        if lease.peer_id != peer_id
            || lease.collection_id != collection_id
            || lease.connection_generation != connection_generation
        {
            return Err(
                contract_error(BleErrorCode::ProtocolMalformed, BleErrorDomain::Gatt, OP)
                    .with_detail("ordered source lease identity does not match the reserved group"),
            );
        }
        state.observed_source_generation = Some(lease.source_generation.clone());
        state.source = Some(OrderedSourceCleanup::Lease(lease.clone()));
        if lease.ingress == crate::boundary::OrderedSourceIngress::BackendOwned {
            self.inner
                .boundary
                .attach_ordered_wake(&lease, state.data_ready.clone())?;
        }

        if state.closing || state.retired || state.setup_error.is_some() {
            self.recover_mobile_ordered_owner(&mut state, &lease)
                .await?;
            return Err(contract_error(
                BleErrorCode::GattStaleHandle,
                BleErrorDomain::Gatt,
                OP,
            ));
        }
        if lease.members.len() != state.claims.len()
            || lease
                .members
                .iter()
                .any(|(id, _)| !state.claims.iter().any(|claim| claim.member_id == *id))
            || lease
                .members
                .iter()
                .map(|(id, _)| id)
                .collect::<HashSet<_>>()
                .len()
                != lease.members.len()
        {
            return Err(
                contract_error(BleErrorCode::ProtocolMalformed, BleErrorDomain::Gatt, OP)
                    .with_detail("source membership identities differ from prepared claims"),
            );
        }
        if lease.ingress == crate::boundary::OrderedSourceIngress::CentralRetainedBatch {
            use ubm_core::ordered_notifications::{Member, OrderedCollection};
            if lease.members.len() != state.claims.len() {
                return Err(contract_error(
                    BleErrorCode::ProtocolMalformed,
                    BleErrorDomain::Gatt,
                    OP,
                )
                .with_detail("source membership count differs from prepared claims"));
            }
            let mut source_members = Vec::with_capacity(lease.members.len());
            for (member_id, membership) in &lease.members {
                let claim = state
                    .claims
                    .iter()
                    .find(|claim| claim.member_id == *member_id)
                    .ok_or_else(|| {
                        contract_error(BleErrorCode::ProtocolMalformed, BleErrorDomain::Gatt, OP)
                    })?;
                let delivery = match claim.delivery {
                    Some(DeliveryMode::Notification) => OrderedDelivery::Notification,
                    Some(DeliveryMode::Indication) => OrderedDelivery::Indication,
                    None => OrderedDelivery::Either,
                };
                source_members.push(Member {
                    id: *member_id,
                    source: *membership,
                    delivery,
                });
            }
            let collection = OrderedCollection::new(
                connection_generation.clone(),
                database_generation.clone(),
                lease.source_generation.clone(),
                collection_id,
                source_members.clone(),
                max_items,
                max_bytes,
                max_controls,
            )
            .map_err(|error| {
                contract_error(BleErrorCode::ProtocolMalformed, BleErrorDomain::Gatt, OP)
                    .with_detail(format!("{error:?}"))
            })?;
            let admission = state.admission.as_ref().ok_or_else(|| {
                contract_error(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Gatt,
                    OP,
                )
            })?;
            if let Err(primary) = self
                .install_mobile_ordered_ingress(ordered_source_key(&lease), collection, admission)
                .await
            {
                if primary.code() == BleErrorCode::GattStaleHandle {
                    state.setup_error = Some(primary.clone());
                    let quarantine = OrderedCollection::new(
                        connection_generation.clone(),
                        database_generation.clone(),
                        lease.source_generation.clone(),
                        collection_id,
                        source_members,
                        max_items,
                        max_bytes,
                        max_controls,
                    )
                    .map_err(|error| {
                        contract_error(BleErrorCode::ProtocolMalformed, BleErrorDomain::Gatt, OP)
                            .with_detail(format!("{error:?}"))
                    })?;
                    if let Err(error) = self
                        .quarantine_mobile_ordered_ingress(ordered_source_key(&lease), quarantine)
                    {
                        state.fence_error = Some(error);
                        return Err(primary);
                    }
                    state.central_source_installed = true;
                    lock_std(&self.inner.mobile_ordered_ingress)
                        .set_wake(
                            lease.collection_id,
                            &ordered_source_key(&lease),
                            state.data_ready.clone(),
                        )
                        .map_err(|e| {
                            DesktopError::subscribe_failed(&format!("ordered wake: {e:?}"))
                        })?;
                    if let Err(error) = self.inner.boundary.ordered_source_installed(&lease).await {
                        state.fence_error = Some(error);
                    } else {
                        state.source_host_attached = true;
                    }
                }
                return Err(primary);
            }
            state.central_source_installed = true;
            lock_std(&self.inner.mobile_ordered_ingress)
                .set_wake(
                    lease.collection_id,
                    &ordered_source_key(&lease),
                    state.data_ready.clone(),
                )
                .map_err(|e| DesktopError::subscribe_failed(&format!("ordered wake: {e:?}")))?;
            self.inner.boundary.ordered_source_installed(&lease).await?;
            state.source_host_attached = true;
        }
        for (member_id, membership) in &lease.members {
            {
                let core = self.inner.core.lock().await;
                if core.connection_generation(&peer_key).as_deref() != Some(&connection_generation)
                    || core.database_generation(&peer_key).as_deref() != Some(&database_generation)
                {
                    return Err(contract_error(
                        BleErrorCode::GattStaleHandle,
                        BleErrorDomain::Gatt,
                        OP,
                    ));
                }
            }
            let activation = state
                .claims
                .iter()
                .find(|claim| claim.member_id == *member_id)
                .and_then(|claim| claim.activation.clone())
                .ok_or_else(|| {
                    contract_error(BleErrorCode::ProtocolMalformed, BleErrorDomain::Gatt, OP)
                })?;
            state.source_claims_started.insert(*member_id);
            match drive_link(
                &self.inner,
                peer_id,
                OP,
                &ctl.ticket,
                window,
                self.inner.boundary.retain_ordered_source_claim(
                    &lease,
                    *member_id,
                    *membership,
                    &activation,
                ),
            )
            .await
            {
                Wait::Done(result) => result?,
                Wait::Expired => return Err(timed_out(OP, window)),
                Wait::Cancelled => return Err(ctl.ticket.interruption(OP)),
            }
        }
        let core = self.inner.core.lock().await;
        if core.connection_generation(&peer_key).as_deref() != Some(&connection_generation)
            || core.database_generation(&peer_key).as_deref() != Some(&database_generation)
        {
            return Err(contract_error(
                BleErrorCode::GattStaleHandle,
                BleErrorDomain::Gatt,
                OP,
            ));
        }
        Ok(lease)
    }
}

impl<B: RadioBoundary> DesktopCentral<B> {
    /// Enable the prepared claims in member order. No per-member value queue
    /// participates in the ordered stream.
    pub(super) async fn enable_ordered_batch(
        &self,
        peer_id: &str,
        collection_id: u64,
        ctl: OpControl,
    ) -> Result<(), DesktopError> {
        let result = self
            .enable_ordered_batch_inner(peer_id, collection_id, &ctl)
            .await;
        if let Err(error) = &result {
            if let Some(owner) = self.inner.ordered_batches.get(peer_id, collection_id) {
                let mut state = owner.lock().await;
                if state.setup_error.is_none() {
                    state.setup_error = Some(error.clone());
                }
            }
        }
        result
    }

    async fn enable_ordered_batch_inner(
        &self,
        peer_id: &str,
        collection_id: u64,
        ctl: &OpControl,
    ) -> Result<(), DesktopError> {
        use crate::boundary::{OrderedSourceCleanup, OrderedSourceTransition};
        const OP: &str = "gatt.ordered.enable";
        let owner = self
            .inner
            .ordered_batches
            .get(peer_id, collection_id)
            .ok_or_else(|| {
                contract_error(BleErrorCode::OwnershipDenied, BleErrorDomain::Gatt, OP)
            })?;
        let mut state = owner.lock().await;
        let lease = match &state.source {
            Some(OrderedSourceCleanup::Lease(lease)) => lease.clone(),
            _ => {
                return Err(contract_error(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Gatt,
                    OP,
                ));
            }
        };
        let peer_key = self.known_peer_key(peer_id).await?;
        let connection_generation = state.connection_generation.clone();
        let database_generation = state.database_generation.clone();
        let window = ctl.budget.window(LIVENESS_OP);
        let admission = state.admission.clone().ok_or_else(|| {
            contract_error(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Gatt,
                OP,
            )
        })?;
        let mut starting = state.starting.take();
        for claim in &mut state.claims {
            self.refuse_before_admission(ctl, OP)?;
            {
                let core = self.inner.core.lock().await;
                if core.connection_generation(&peer_key) != connection_generation
                    || core.database_generation(&peer_key) != database_generation
                {
                    return Err(contract_error(
                        BleErrorCode::GattStaleHandle,
                        BleErrorDomain::Gatt,
                        OP,
                    ));
                }
            }
            let prepared = claim.activation.as_ref().ok_or_else(|| {
                contract_error(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Gatt,
                    OP,
                )
            })?;
            self.update_ordered_owner(
                &lease,
                OrderedSourceTransition::BeginEnable {
                    member_id: claim.member_id,
                },
            )?;
            // On failure record only the enable outcome here. The setup error
            // is retained by the batch and supplied to source retirement; the
            // source must order its own terminal after any admitted records.
            if !claim.drive_enable {
                // A shared hub may still be enabling. Wait for its actual
                // consumer state instead of treating a join as ready.
                starting.take();
                let wait_ready = async {
                    loop {
                        let core = self.inner.core.lock().await;
                        if core.connection_generation(&peer_key) != connection_generation
                            || core.database_generation(&peer_key) != database_generation
                        {
                            return Err(contract_error(
                                BleErrorCode::GattStaleHandle,
                                BleErrorDomain::Gatt,
                                OP,
                            ));
                        }
                        match core.consumer_state(claim.path_index, &claim.consumer) {
                            Some(ubm_core::central::ConsumerState::Ready) => return Ok(()),
                            Some(ubm_core::central::ConsumerState::Enabling) => {}
                            _ => {
                                return Err(contract_error(
                                    BleErrorCode::PlatformFailure,
                                    BleErrorDomain::Gatt,
                                    OP,
                                )
                                .with_detail("shared notification enable did not succeed"));
                            }
                        }
                        drop(core);
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                };
                let ready =
                    match drive_link(&self.inner, peer_id, OP, &ctl.ticket, window, wait_ready)
                        .await
                    {
                        Wait::Done(result) => result,
                        Wait::Expired => Err(timed_out(OP, window)),
                        Wait::Cancelled => Err(ctl.ticket.interruption(OP)),
                    };
                if let Err(error) = ready {
                    self.update_ordered_owner(
                        &lease,
                        OrderedSourceTransition::EnableResult {
                            member_id: claim.member_id,
                            outcome: Some(false),
                        },
                    )?;
                    return Err(error);
                }
                let observed = lock_std(&self.inner.deliveries)
                    .get(&claim.scope)
                    .copied()
                    .unwrap_or(ObservedDelivery::Unknown);
                if let Some(required) = claim.delivery {
                    if !observed.satisfies(required) {
                        let error = Self::delivery_unenforced(
                            required,
                            observed,
                            "a join cannot rewrite the live CCCD",
                        );
                        self.update_ordered_owner(
                            &lease,
                            OrderedSourceTransition::EnableResult {
                                member_id: claim.member_id,
                                outcome: Some(false),
                            },
                        )?;
                        return Err(error);
                    }
                }
                let mut core = self.inner.core.lock().await;
                report_terminal_release(&mut core, &claim.operation, true, None);
                recycle_observations(&mut core);
                claim.phase = OrderedClaimPhase::Acquired;
                claim.ticket.settle();
                self.update_ordered_owner(
                    &lease,
                    OrderedSourceTransition::EnableResult {
                        member_id: claim.member_id,
                        outcome: Some(true),
                    },
                )?;
                continue;
            }
            if starting.is_none() {
                starting = Some(
                    match drive_link(
                        &self.inner,
                        peer_id,
                        OP,
                        &ctl.ticket,
                        window,
                        admission.begin_starting(),
                    )
                    .await
                    {
                        Wait::Done(result) => result?,
                        Wait::Expired => return Err(timed_out(OP, window)),
                        Wait::Cancelled => return Err(ctl.ticket.interruption(OP)),
                    },
                );
            }
            admission.assert_current()?;
            claim.phase = OrderedClaimPhase::Dispatching;
            let membership = lease
                .members
                .iter()
                .find(|(id, _)| *id == claim.member_id)
                .map(|(_, membership)| *membership)
                .ok_or_else(|| {
                    contract_error(BleErrorCode::ProtocolMalformed, BleErrorDomain::Gatt, OP)
                })?;
            {
                let mut origins = lock_std(&self.inner.ordered_physical_origins);
                let key = (claim.scope.clone(), claim.epoch);
                if origins.contains_key(&key) {
                    return Err(contract_error(
                        BleErrorCode::LifecycleInvalidState,
                        BleErrorDomain::Gatt,
                        OP,
                    )
                    .with_detail("ordered physical activation already has a driver"));
                }
                self.inner.boundary.reserve_ordered_notification(
                    &lease,
                    claim.member_id,
                    membership,
                    prepared,
                )?;
                origins.insert(
                    key,
                    crate::boundary::OrderedPhysicalOrigin {
                        lease: lease.clone(),
                        member_id: claim.member_id,
                        membership,
                        prepared: prepared.clone(),
                    },
                );
            }
            let native = self.inner.boundary.dispatch_ordered_notification(
                &lease,
                claim.member_id,
                membership,
                prepared,
            );
            let mut native = std::pin::pin!(native);
            let mut first_poll_guard = starting.take();
            let dispatch = std::future::poll_fn(move |context| {
                let result = native.as_mut().poll(context);
                first_poll_guard.take();
                result
            });
            let answer =
                match drive_link(&self.inner, peer_id, OP, &ctl.ticket, window, dispatch).await {
                    Wait::Done(answer) => answer,
                    Wait::Expired => Err(timed_out(OP, window)),
                    Wait::Cancelled => Err(ctl.ticket.interruption(OP)),
                };
            match answer {
                Ok(observed)
                    if claim
                        .delivery
                        .is_none_or(|required| observed.satisfies(required)) =>
                {
                    let settled = self
                        .settle_enable_success(
                            peer_id,
                            &claim.scope,
                            claim.path_index,
                            &claim.operation,
                            claim.epoch,
                            observed,
                        )
                        .await;
                    match settled {
                        Ok(_) => {
                            claim.phase = OrderedClaimPhase::Acquired;
                            claim.ticket.settle();
                            self.update_ordered_owner(
                                &lease,
                                OrderedSourceTransition::EnableResult {
                                    member_id: claim.member_id,
                                    outcome: Some(true),
                                },
                            )?;
                        }
                        Err(error) => {
                            claim.phase = OrderedClaimPhase::Indeterminate;
                            self.update_ordered_owner(
                                &lease,
                                OrderedSourceTransition::EnableResult {
                                    member_id: claim.member_id,
                                    outcome: None,
                                },
                            )?;
                            return Err(error);
                        }
                    }
                }
                other => {
                    let error = match other {
                        Ok(observed) => Self::delivery_unenforced(
                            claim.delivery.expect("guarded requirement"),
                            observed,
                            "the radio did not report the required mode",
                        ),
                        Err(error) => error,
                    };
                    claim.phase = OrderedClaimPhase::Indeterminate;
                    self.update_ordered_owner(
                        &lease,
                        OrderedSourceTransition::EnableResult {
                            member_id: claim.member_id,
                            outcome: None,
                        },
                    )?;
                    return Err(error);
                }
            }
        }
        {
            let core = self.inner.core.lock().await;
            if core.connection_generation(&peer_key) != connection_generation
                || core.database_generation(&peer_key) != database_generation
            {
                return Err(contract_error(
                    BleErrorCode::GattStaleHandle,
                    BleErrorDomain::Gatt,
                    OP,
                ));
            }
        }
        admission.mark_dispatched();
        drop(starting);
        Ok(())
    }
}
