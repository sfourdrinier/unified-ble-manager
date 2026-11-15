// bindings/jni/src/mobile/ordered_test_harness.rs
use super::*;

fn verify_retirement(
    env: &mut Env,
    target: &JObject,
    result: &JObject,
    expected: jint,
) -> MobileResult<jint> {
    let version = if expected == 10 { 3 } else { 4 };
    let wrong_owner = expected == 11;
    let owner = ubm_mobile::OrderedAdmissionOwner::negotiate_slot(
        2,
        "12345678-1234-4abc-8def-123456789abc",
    )
    .unwrap();
    let retirement_id = ubm_mobile::OrderedAdmissionOwner::negotiate_slot(
        2,
        "22345678-1234-4abc-8def-123456789abc",
    )
    .unwrap();
    let other = ubm_mobile::OrderedAdmissionOwner::negotiate_slot(
        2,
        "12345678-1234-4abc-8def-123456789abd",
    )
    .unwrap();
    let fence = RadioRequest::OwnerFence {
        id: 10,
        owner_incarnation: owner,
        retirement_id,
    };
    let status = RadioRequest::OwnerStatus {
        id: 11,
        owner_incarnation: owner,
        retirement_id,
    };
    let valid = validate_dispatch(&fence, version, if wrong_owner { other } else { owner });
    if version != 4 || wrong_owner {
        return Ok(jint::from(valid.is_err()));
    }
    if valid.is_err() {
        return Ok(0);
    }
    for request in [&fence, &status] {
        let (method, args) = request_call(request);
        call_host(env, target, method, &args)?;
    }
    let completion = owner_retirement_completion(env, result)?;
    let exact = match &completion {
        RadioCompletion::OwnerRetirement(ubm_mobile::OwnerRetirementResult {
            state: ubm_mobile::OwnerRetirementState::Quiescent,
            ..
        }) => true,
        RadioCompletion::OwnerRetirement(ubm_mobile::OwnerRetirementResult {
            state:
                ubm_mobile::OwnerRetirementState::Rejected {
                    reason: ubm_mobile::OwnerRetirementRejection::ConflictingRetirement,
                },
            ..
        }) => true,
        RadioCompletion::OwnerRetirement(ubm_mobile::OwnerRetirementResult {
            state:
                ubm_mobile::OwnerRetirementState::FencedPending {
                    outstanding_classes,
                    failures,
                },
            ..
        }) => {
            outstanding_classes == &[ubm_mobile::OwnerOutstandingClass::Callbacks]
                && failures.len() == 1
                && failures[0].kind == FailureKind::Busy
                && failures[0].gatt_status == Some(7)
                && failures[0].native_domain.as_deref() == Some("native-domain")
                && failures[0].native_code == Some(-42)
                && failures[0].detail == "retained failure"
                && !failures[0].dispatched
        }
        _ => false,
    };
    Ok(jint::from(
        completion.answers_request(&fence) && completion.answers_request(&status) && exact,
    ))
}

// Synthetic fixture for the v2 object constructor and completion decoder.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_TestSlotWire_nativeVerify<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    target: JObject<'caller>,
    result: JObject<'caller>,
    expected: jint,
) -> jint {
    unowned_env.with_env(|env| -> MobileResult<jint> {
        if expected >= 10 { return verify_retirement(env, &target, &result, expected); }
        let owner = ubm_mobile::OrderedAdmissionOwner::negotiate_slot(2, "12345678-1234-4abc-8def-123456789abc").unwrap();
        let identity = ubm_mobile::OrderedOpenIdentity {
            version: 2, owner_incarnation: owner, slot: 3, slot_generation: u64::MAX, original_request_id: 7, native_group_id: u64::MAX, peer_id: "peer".into(),
            spec: ubm_mobile::OrderedOpenSpec { connection_generation: "conn".into(), database_generation: "db".into(), snapshot_identity: ubm_desktop::boundary::GattSnapshotIdentity { owner: "graph".into(), attachment: u64::MAX, revision: 9 }, members: vec![ubm_mobile::OrderedMember { member_id: u64::MAX, instance: Instance { peer_id: "peer".into(), service_uuid: "service".into(), service_occurrence: u64::MAX, characteristic_uuid: "characteristic".into(), characteristic_occurrence: 9 } }], item_capacity: 4, byte_capacity: 32 },
        };
        let request = match expected {
            0 | 2 => ubm_mobile::OrderedOpenRequestV2::Open { id: 7, identity },
            _ => ubm_mobile::OrderedOpenRequestV2::Acknowledge { id: 8, identity },
        };
        let radio_request = RadioRequest::OrderedOpenV2(request);
        let (method, args) = request_call(&radio_request);
        call_host(env, &target, method, &args)?;
        let accepted = ordered_completion(env, &result).is_ok_and(|completion| {
            if expected == 2 {
                matches!(&completion, RadioCompletion::OrderedOpenV2(ubm_mobile::OrderedOpenCompletionV2::Result(ubm_mobile::OrderedOpenResultV2 {
                    outcome: ubm_mobile::OrderedOpenOutcomeV2::Failed { original_failure, ownership: ubm_mobile::OrderedOpenOwnershipV2::Created { .. } }, ..
                })) if original_failure.native_domain.as_deref() == Some("native-domain") && original_failure.native_code == Some(-42) && original_failure.detail == "original failure")
                    && completion.answers_request(&radio_request)
            } else { completion.answers_request(&radio_request) }
        });
        Ok(jint::from(accepted))
    }).resolve_with::<ThrowMobile, _>(|| "mobile.test.slot-wire")
}

// Compiled only by the synthetic JVM harness; absent from shipped binaries.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_TestOrdered_nativeExercise<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    target: JObject<'caller>,
) -> jint {
    unowned_env
        .with_env(|env| -> MobileResult<jint> {
            use ubm_mobile::{
                OrderedEnd as E, OrderedLossScope as S, OrderedLossUnit as U, OrderedMember,
                OrderedRecord as R, OrderedRegistration, OrderedRejected as X,
                OrderedSourceGeneration as G,
            };
            let platform = Arc::new(JniRadio {
                vm: env.get_java_vm()?,
                host: env.new_global_ref(&target)?,
                owner: OnceLock::new(),
                admission_owner: ubm_mobile::OrderedAdmissionOwner::negotiate(1, "12345678-1234-4abc-8def-123456789abc").unwrap(),
                retirement_owner: None,
                admission_version: 1,
                retirement_version: 0,
                subrate_available: false,
            });
            let radio =
                ubm_mobile::ForeignRadio::new(platform, MobilePlatform::Android, "jni-test".into());
            TEST_RADIO.with(|slot| *slot.borrow_mut() = Some(radio.clone()));
            let host =
                current_host().ok_or_else(|| invalid("mobile.test.ordered", "missing host"))?;
            host.ordered_wakes()
                .begin(u64::MAX)
                .map_err(|_| invalid("mobile.test.ordered", "wake reservation failed"))?;
            let result = ubm_desktop::executor::desktop_runtime().block_on(async {
                let member = OrderedRegistration {
                    member_id: u64::MAX,
                    source_member: u64::MAX,
                    epoch: u64::MAX,
                };
                let generation = G::Counter(u64::MAX);
                let discovered = radio.call(|id| RadioRequest::Discover { id, peer_id: "peer".into() }).await;
                if !matches!(discovered, Ok(RadioCompletion::DiscoveredScoped { identity, .. }) if identity.owner == "graph" && identity.attachment == u64::MAX && identity.revision == u64::MAX) { return false; }
                let activation = ubm_mobile::OrderedActivation { instance: Instance { peer_id: "peer".into(), service_uuid: "service".into(), service_occurrence: u64::MAX, characteristic_uuid: "characteristic".into(), characteristic_occurrence: u64::MAX }, connection_generation: "connection".into(), source_generation: generation.clone(), group_id: u64::MAX, member, activation_epoch: u64::MAX };
                if radio.call(|id| RadioRequest::OrderedEnableNotifications { id, activation: activation.clone(), requested: None, preferred: None }).await != Ok(RadioCompletion::NotifyEnabled(ObservedDelivery::Unknown)) { return false; }
                let released = radio.call(|id| RadioRequest::OrderedEnableNotifications { id, activation: activation.clone(), requested: None, preferred: None }).await;
                if !matches!(released, Ok(RadioCompletion::OrderedEnableReleased(failure)) if failure.kind == FailureKind::PathStale && failure.gatt_status == Some(133)) { return false; }
                if radio.call(|id| RadioRequest::OrderedEnableSettlement { id, activation: activation.clone() }).await != Ok(RadioCompletion::OrderedEnableStatus(ubm_mobile::OrderedEnableStatus::Pending)) { return false; }
                if radio.call(|id| RadioRequest::OrderedEnableSettlement { id, activation: activation.clone() }).await != Ok(RadioCompletion::OrderedEnableStatus(ubm_mobile::OrderedEnableStatus::Enabled(ObservedDelivery::Indication))) { return false; }
                if radio.call(|id| RadioRequest::OrderedEnableSettlement { id, activation: activation.clone() }).await != Ok(RadioCompletion::OrderedEnableStatus(ubm_mobile::OrderedEnableStatus::Released(None))) { return false; }
                let uncertain = radio.call(|id| RadioRequest::OrderedEnableSettlement { id, activation: activation.clone() }).await;
                if !matches!(uncertain, Ok(RadioCompletion::OrderedEnableStatus(ubm_mobile::OrderedEnableStatus::Uncertain(failure))) if failure.kind == FailureKind::PathStale && failure.gatt_status == Some(133) && failure.native_name.as_deref() == Some("native-code")) { return false; }
                if radio.call(|id| RadioRequest::OrderedDisableNotifications { id, activation: activation.clone() }).await != Ok(RadioCompletion::Unit) { return false; }
                if radio.call(|id| RadioRequest::OrderedForgetEnableReceipt { id, activation }).await != Ok(RadioCompletion::Unit) { return false; }
                let opened = radio
                    .call(|id| RadioRequest::OrderedOpen {
                        id,
                        group_id: u64::MAX,
                        connection_generation: "connection".into(), database_generation: "database".into(), snapshot_identity: ubm_desktop::boundary::GattSnapshotIdentity { owner: "graph".into(), attachment: u64::MAX, revision: u64::MAX },
                        members: vec![OrderedMember {
                            member_id: u64::MAX,
                            instance: Instance {
                                peer_id: "peer".into(),
                                service_uuid: "service".into(),
                                service_occurrence: u64::MAX,
                                characteristic_uuid: "characteristic".into(),
                                characteristic_occurrence: u64::MAX,
                            },
                        }],
                        item_capacity: 3,
                        byte_capacity: 8,
                    })
                    .await;
                if opened
                    != Ok(RadioCompletion::OrderedOpened {
                        generation: generation.clone(),
                        members: vec![member],
                    })
                {
                    return false;
                }
                if host
                    .ordered_wakes()
                    .bind(u64::MAX, generation.clone())
                    .is_err()
                {
                    return false;
                }
                let Some(ready) = host.ordered_wakes().take() else {
                    return false;
                };
                if ready.group_id != u64::MAX || ready.generation != generation {
                    return false;
                }
                if host.ordered_wakes().finish(&ready).is_err() {
                    return false;
                }
                for attempt in 0..3 {
                    let result = radio.call(|id| RadioRequest::OrderedMarkerWriteSettlement { id, generation: generation.clone(), group_id: u64::MAX, marker_id: u64::MAX }).await;
                    match result {
                        Ok(RadioCompletion::OrderedMarkerWriteStatus(status)) if attempt == 0 && matches!(status.admission, ubm_mobile::OrderedMarkerAdmission::Pending) && matches!(status.write, ubm_mobile::OrderedMarkerWriteState::NotStarted { failure: None }) => (),
                        Ok(RadioCompletion::OrderedMarkerWriteStatus(status)) if attempt == 2 && matches!(status.admission, ubm_mobile::OrderedMarkerAdmission::Admitted { ordinal: u64::MAX }) && matches!(status.write, ubm_mobile::OrderedMarkerWriteState::Started { failure: Some(ref error) } if error.dispatched && error.native_name.as_deref() == Some("dispatch-error")) => (),
                        Ok(RadioCompletion::OrderedMarkerWriteStatus(status)) if attempt == 1 && matches!(status.admission, ubm_mobile::OrderedMarkerAdmission::Admitted { ordinal: u64::MAX }) && matches!(status.write, ubm_mobile::OrderedMarkerWriteState::Initiating) => (),
                        _ => return false,
                    }
                }
                let cancelled = radio.call(|id| RadioRequest::OrderedCancelMarkerWrite { id, generation: generation.clone(), group_id: u64::MAX, marker_id: u64::MAX }).await;
                if !matches!(cancelled, Ok(RadioCompletion::OrderedMarkerWriteStatus(ubm_mobile::OrderedMarkerWriteStatus { admission: ubm_mobile::OrderedMarkerAdmission::NotAdmitted, write: ubm_mobile::OrderedMarkerWriteState::NotStarted { failure: None } }))) { return false; }
                let expected = RadioCompletion::OrderedRecords {
                    batch_id: Some(u64::MAX),
                    records: vec![R::Marker { marker_id: u64::MAX, ordinal: 1u64 << 63 }, R::SourceFailed {
                        uncertain_tail: true,
                        failure: PlatformFailure::new(FailureKind::PermissionDenied, "Bluetooth permission denied while maintaining notification source"),
                        native_domain: None,
                        native_code: None,
                        failure_member: Some(member),
                        rejected: Some(X::Loss {
                            scope: S::Member(u64::MAX),
                            unit: U::SourceRecords,
                            count: Some(u64::MAX),
                            byte_count: None,
                        }),
                    }, R::SourceFailed { uncertain_tail: true, failure: PlatformFailure { gatt_status: Some(42), ..PlatformFailure::new(FailureKind::GattStatus, "Android notification source GATT failure") }, native_domain: Some("android".into()), native_code: Some("42".into()), failure_member: Some(member), rejected: None }, R::SubsequentSourceFailure { failure: PlatformFailure { gatt_status: Some(257), dispatched: true, ..PlatformFailure::new(FailureKind::GattStatus, "Android notification source failure") } }, R::FailureObservationsOmitted, R::Terminal { reason: E::Overflow, uncertain_tail: false, native_domain: None, native_code: None, failure_member: None, rejected: Some(X::Marker { marker_id: u64::MAX }) }],
                };
                for _ in 0..2 {
                    if radio
                        .call(|id| RadioRequest::OrderedDrain {
                            id,
                            generation: generation.clone(),
                            group_id: u64::MAX,
                        })
                        .await
                        != Ok(expected.clone())
                    {
                        return false;
                    }
                }
                if radio
                    .call(|id| RadioRequest::OrderedAcknowledgeBatch {
                        id,
                        generation: generation.clone(),
                        group_id: u64::MAX,
                        batch_id: u64::MAX,
                    })
                    .await
                    != Ok(RadioCompletion::Unit)
                {
                    return false;
                }
                for group_id in [8, 9, 10] {
                    let failed_open = radio.call(|id| RadioRequest::OrderedOpen { id, group_id, connection_generation: "connection".into(), database_generation: "database".into(), snapshot_identity: ubm_desktop::boundary::GattSnapshotIdentity { owner: "graph".into(), attachment: u64::MAX, revision: u64::MAX }, members: vec![OrderedMember { member_id: u64::MAX, instance: Instance { peer_id: "peer".into(), service_uuid: "service".into(), service_occurrence: 0, characteristic_uuid: "characteristic".into(), characteristic_occurrence: 0 } }], item_capacity: 3, byte_capacity: 8 }).await;
                    let expected = match group_id { 8 => ubm_mobile::OrderedOpenOwnership::Indeterminate { recovery_key: u64::MAX }, 9 => ubm_mobile::OrderedOpenOwnership::NotCreated, _ => ubm_mobile::OrderedOpenOwnership::Created { generation: generation.clone(), members: vec![member] } };
                    if !matches!(failed_open, Ok(RadioCompletion::OrderedOpenFailed { failure, ownership }) if failure.kind == FailureKind::PathStale && ownership == expected) { return false; }
                }
                if radio.call(|id| RadioRequest::OrderedRecoverOpen { id, group_id: u64::MAX, open_request_id: u64::MAX - 1, peer_id: "peer".into() }).await != Ok(RadioCompletion::OrderedOpened { generation: generation.clone(), members: vec![member] }) { return false; }
                let stale = OrderedRegistration {
                    source_member: u64::MAX - 1,
                    ..member
                };
                if radio
                    .call(|id| RadioRequest::OrderedRetainClaim {
                        id,
                        generation: generation.clone(),
                        group_id: u64::MAX,
                        member: stale,
                    })
                    .await
                    .is_ok()
                {
                    return false;
                }
                if radio
                    .call(|id| RadioRequest::OrderedRetainClaim {
                        id,
                        generation: generation.clone(),
                        group_id: u64::MAX,
                        member,
                    })
                    .await
                    != Ok(RadioCompletion::OrderedClaim {
                        generation: generation.clone(),
                        member,
                    })
                {
                    return false;
                }
                // Drop the open waiter before the platform answers. Its cancellation callback
                // completes the installed source late, so only retained ownership can recover it.
                use std::future::Future;
                let mut opening = Box::pin(radio.call(|id| RadioRequest::OrderedOpen {
                    id,
                    group_id: 7,
                    connection_generation: "connection".into(), database_generation: "database".into(), snapshot_identity: ubm_desktop::boundary::GattSnapshotIdentity { owner: "graph".into(), attachment: u64::MAX, revision: u64::MAX },
                    members: vec![OrderedMember {
                        member_id: u64::MAX,
                        instance: Instance {
                            peer_id: "peer".into(),
                            service_uuid: "service".into(),
                            service_occurrence: u64::MAX,
                            characteristic_uuid: "characteristic".into(),
                            characteristic_occurrence: u64::MAX,
                        },
                    }],
                    item_capacity: 3,
                    byte_capacity: 8,
                }));
                let mut context = std::task::Context::from_waker(std::task::Waker::noop());
                if !matches!(
                    opening.as_mut().poll(&mut context),
                    std::task::Poll::Pending
                ) {
                    return false;
                }
                drop(opening);
                let Some(retained) = radio.retained_ordered_open(7) else {
                    return false;
                };
                if retained.completion
                    != Some(RadioCompletion::OrderedOpened {
                        generation: generation.clone(),
                        members: vec![member],
                    })
                {
                    return false;
                }
                if radio
                    .call(|id| RadioRequest::OrderedBarrier {
                        id,
                        generation: generation.clone(),
                        group_id: 7,
                        barrier_id: u64::MAX - 1,
                    })
                    .await
                    != Ok(RadioCompletion::Unit)
                {
                    return false;
                }
                let mut barrier = Box::pin(radio.call(|id| RadioRequest::OrderedBarrier {
                    id,
                    generation: generation.clone(),
                    group_id: 7,
                    barrier_id: u64::MAX,
                }));
                if !matches!(
                    barrier.as_mut().poll(&mut context),
                    std::task::Poll::Pending
                ) {
                    return false;
                }
                drop(barrier);
                let value = RadioCompletion::OrderedRecords {
                    batch_id: Some(9),
                    records: vec![
                        R::Value {
                            member,
                            ordinal: 1,
                            value: vec![0, 128, 255],
                            provenance: ubm_mobile::OrderedProvenance::Notification,
                            delivery: ObservedDelivery::Unknown,
                        },
                        R::Barrier {
                            barrier_id: u64::MAX - 1,
                            uncertain_tail: false,
                        },
                        R::Value {
                            member,
                            ordinal: 2,
                            value: vec![7],
                            provenance: ubm_mobile::OrderedProvenance::Notification,
                            delivery: ObservedDelivery::Unknown,
                        },
                        R::Barrier {
                            barrier_id: u64::MAX,
                            uncertain_tail: true,
                        },
                    ],
                };
                if radio
                    .call(|id| RadioRequest::OrderedDrain {
                        id,
                        generation: generation.clone(),
                        group_id: 7,
                    })
                    .await
                    != Ok(value)
                {
                    return false;
                }
                if radio
                    .call(|id| RadioRequest::OrderedAcknowledgeBatch {
                        id,
                        generation: generation.clone(),
                        group_id: 7,
                        batch_id: 9,
                    })
                    .await
                    != Ok(RadioCompletion::Unit)
                {
                    return false;
                }
                if radio
                    .call(|id| RadioRequest::OrderedClose {
                        id,
                        generation: generation.clone(),
                        group_id: 7,
                    })
                    .await
                    != Ok(RadioCompletion::Unit)
                {
                    return false;
                }
                let terminal = RadioCompletion::OrderedRecords {
                    batch_id: Some(10),
                    records: vec![R::Terminal {
                        reason: E::Overflow,
                        uncertain_tail: true,
                        native_domain: None,
                        native_code: None,
                        failure_member: None,
                        rejected: Some(X::Barrier {
                            barrier_id: u64::MAX - 1,
                        }),
                    }],
                };
                if radio
                    .call(|id| RadioRequest::OrderedDrain {
                        id,
                        generation: generation.clone(),
                        group_id: 7,
                    })
                    .await
                    != Ok(terminal)
                {
                    return false;
                }
                if radio
                    .call(|id| RadioRequest::OrderedAcknowledgeBatch {
                        id,
                        generation: generation.clone(),
                        group_id: 7,
                        batch_id: 10,
                    })
                    .await
                    != Ok(RadioCompletion::Unit)
                {
                    return false;
                }
                let Some(open) = radio.retained_ordered_open(7) else { return false; };
                if radio.call(|id| RadioRequest::OrderedAcknowledgeOpen {
                    id, version: ubm_mobile::ORDERED_ACKNOWLEDGE_OPEN_VERSION,
                    group_id: 7, open_request_id: open.request_id, peer_id: open.peer_id,
                }).await != Ok(RadioCompletion::Unit) { return false; }
                if radio
                    .call(|id| RadioRequest::OrderedRemove {
                        id,
                        generation: generation.clone(),
                        group_id: 7,
                    })
                    .await
                    != Ok(RadioCompletion::Unit)
                {
                    return false;
                }
                if radio
                    .call(|id| RadioRequest::OrderedForgetRemovalReceipt {
                        id,
                        generation: generation.clone(),
                        group_id: 7,
                    })
                    .await
                    != Ok(RadioCompletion::Unit)
                {
                    return false;
                }
                let stale_register = radio.call(|id| RadioRequest::OrderedRegister { id, generation: generation.clone(), group_id: u64::MAX, member: stale }).await;
                if !matches!(stale_register, Err(ref error) if error.code_str() == "gatt.stale-handle") { return false; }
                let stale_retire = radio.call(|id| RadioRequest::OrderedRetire { id, generation: generation.clone(), group_id: u64::MAX, member: stale }).await;
                if !matches!(stale_retire, Err(ref error) if error.code_str() == "gatt.stale-handle") { return false; }
                if radio.call(|id| RadioRequest::OrderedRetire { id, generation: generation.clone(), group_id: u64::MAX, member }).await != Ok(RadioCompletion::Unit) { return false; }
                let renewed = OrderedRegistration { epoch: u64::MAX - 1, ..member };
                radio.call(|id| RadioRequest::OrderedRegister { id, generation, group_id: u64::MAX, member }).await == Ok(RadioCompletion::OrderedRegistered(renewed))
            });
            host.ordered_wakes().remove(u64::MAX);
            TEST_RADIO.with(|slot| *slot.borrow_mut() = None);
            Ok(i32::from(result))
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.test.ordered")
}

// Drive the actual installed JniRadio without claiming shared owner quiescence.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_TestBindingRoute_nativeTestOwnerDispatch<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    binding: jlong,
) -> jint {
    unowned_env
        .with_env(|_env| -> MobileResult<jint> {
            let radio = host_slot()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .test_radios
                .get(&binding)
                .and_then(Weak::upgrade)
                .ok_or_else(|| invalid("mobile.test.owner", "missing installed radio"))?;
            if radio.admission_version != ubm_mobile::ORDERED_ADMISSION_SLOT_VERSION
                || radio.retirement_version != ubm_mobile::ORDERED_OWNER_RETIREMENT_VERSION
            {
                return Ok(0);
            }
            radio.submit(RadioRequest::OwnerFence {
                id: 999,
                owner_incarnation: radio.retirement_owner.unwrap_or(radio.admission_owner),
                retirement_id: retirement_identity(binding)?,
            });
            Ok(1)
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.test.owner")
}
