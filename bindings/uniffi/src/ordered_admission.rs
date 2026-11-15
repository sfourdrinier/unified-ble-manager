//! Checked mapping for the additive private ordered-admission v2 contract.
//! The existing production `PlatformRadio` seam still transports v1 requests.
use super::{
    ordered_generation, MobileGattSnapshotIdentity, MobileInstance, MobileOrderedGeneration,
    MobileOrderedMember, MobileOrderedRegistration,
};
use ubm_mobile::{
    FailureKind, OrderedAdmissionOwner, OrderedMember, OrderedOpenCompletionV2,
    OrderedOpenIdentity, OrderedOpenIndeterminateReason, OrderedOpenOutcomeV2,
    OrderedOpenOwnershipV2, OrderedOpenRequestV2, OrderedOpenResultV2, OrderedOpenRetired,
    OrderedOpenSpec, OrderedSlotIdentity, PlatformFailure,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MobileOrderedOpenSpec {
    pub connection_generation: String,
    pub database_generation: String,
    pub snapshot_identity: MobileGattSnapshotIdentity,
    pub members: Vec<MobileOrderedMember>,
    pub item_capacity: u32,
    pub byte_capacity: u32,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MobileOrderedOpenIdentity {
    pub version: u32,
    pub owner_incarnation: String,
    pub slot: u32,
    pub slot_generation: u64,
    pub original_request_id: u64,
    pub native_group_id: u64,
    pub peer_id: String,
    pub spec: MobileOrderedOpenSpec,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MobileOrderedSlotIdentity {
    pub owner_incarnation: String,
    pub slot: u32,
    pub slot_generation: u64,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MobileOrderedFailureV2 {
    pub kind: String,
    pub gatt_status: Option<i32>,
    pub native_domain: Option<String>,
    pub native_code: Option<i64>,
    pub native_name: Option<String>,
    pub detail: String,
    pub dispatched: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MobileOrderedOwnershipV2 {
    NotCreated,
    Created {
        source_generation: MobileOrderedGeneration,
        members: Vec<MobileOrderedRegistration>,
    },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MobileOrderedIndeterminateReason {
    Failure { failure: MobileOrderedFailureV2 },
    Reason { reason: String },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MobileOrderedOpenOutcome {
    Pending,
    Opened {
        source_generation: MobileOrderedGeneration,
        members: Vec<MobileOrderedRegistration>,
    },
    Failed {
        original_failure: MobileOrderedFailureV2,
        ownership: MobileOrderedOwnershipV2,
    },
    Indeterminate {
        original_failure_or_reason: MobileOrderedIndeterminateReason,
    },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MobileOrderedOpenCompletion {
    Result {
        slot_identity: MobileOrderedSlotIdentity,
        outcome: MobileOrderedOpenOutcome,
    },
    Retired {
        slot_identity: MobileOrderedSlotIdentity,
    },
}

impl From<&OrderedOpenIdentity> for MobileOrderedOpenIdentity {
    fn from(v: &OrderedOpenIdentity) -> Self {
        Self {
            version: v.version,
            owner_incarnation: v.owner_incarnation.to_string(),
            slot: v.slot,
            slot_generation: v.slot_generation,
            original_request_id: v.original_request_id,
            native_group_id: v.native_group_id,
            peer_id: v.peer_id.clone(),
            spec: MobileOrderedOpenSpec {
                connection_generation: v.spec.connection_generation.clone(),
                database_generation: v.spec.database_generation.clone(),
                snapshot_identity: (&v.spec.snapshot_identity).into(),
                members: v
                    .spec
                    .members
                    .iter()
                    .map(|m| MobileOrderedMember {
                        member_id: m.member_id,
                        instance: MobileInstance::from(&m.instance),
                    })
                    .collect(),
                item_capacity: v.spec.item_capacity,
                byte_capacity: v.spec.byte_capacity,
            },
        }
    }
}
impl TryFrom<MobileOrderedOpenIdentity> for OrderedOpenIdentity {
    type Error = String;
    fn try_from(v: MobileOrderedOpenIdentity) -> Result<Self, Self::Error> {
        let result = Self {
            version: v.version,
            owner_incarnation: OrderedAdmissionOwner::negotiate_slot(
                v.version,
                &v.owner_incarnation,
            )
            .map_err(str::to_owned)?,
            slot: v.slot,
            slot_generation: v.slot_generation,
            original_request_id: v.original_request_id,
            native_group_id: v.native_group_id,
            peer_id: v.peer_id,
            spec: OrderedOpenSpec {
                connection_generation: v.spec.connection_generation,
                database_generation: v.spec.database_generation,
                snapshot_identity: v.spec.snapshot_identity.into(),
                members: v
                    .spec
                    .members
                    .into_iter()
                    .map(|m| OrderedMember {
                        member_id: m.member_id,
                        instance: m.instance.into(),
                    })
                    .collect(),
                item_capacity: v.spec.item_capacity,
                byte_capacity: v.spec.byte_capacity,
            },
        };
        result.validate().map_err(str::to_owned)?;
        Ok(result)
    }
}
impl From<OrderedSlotIdentity> for MobileOrderedSlotIdentity {
    fn from(v: OrderedSlotIdentity) -> Self {
        Self {
            owner_incarnation: v.owner_incarnation.to_string(),
            slot: v.slot,
            slot_generation: v.slot_generation,
        }
    }
}
impl TryFrom<MobileOrderedSlotIdentity> for OrderedSlotIdentity {
    type Error = String;
    fn try_from(v: MobileOrderedSlotIdentity) -> Result<Self, String> {
        if v.slot >= 64 || v.slot_generation == 0 {
            return Err("invalid ordered slot identity".into());
        }
        Ok(Self {
            owner_incarnation: OrderedAdmissionOwner::negotiate_slot(2, &v.owner_incarnation)
                .map_err(str::to_owned)?,
            slot: v.slot,
            slot_generation: v.slot_generation,
        })
    }
}
impl From<&PlatformFailure> for MobileOrderedFailureV2 {
    fn from(v: &PlatformFailure) -> Self {
        Self {
            kind: v.kind.as_str().into(),
            gatt_status: v.gatt_status,
            native_domain: v.native_domain.clone(),
            native_code: v.native_code,
            native_name: v.native_name.clone(),
            detail: v.detail.clone(),
            dispatched: v.dispatched,
        }
    }
}
impl TryFrom<MobileOrderedFailureV2> for PlatformFailure {
    type Error = String;
    fn try_from(v: MobileOrderedFailureV2) -> Result<Self, String> {
        Ok(Self {
            kind: FailureKind::parse(&v.kind)
                .ok_or_else(|| format!("unknown failure kind {}", v.kind))?,
            gatt_status: v.gatt_status,
            native_domain: v.native_domain,
            native_code: v.native_code,
            native_name: v.native_name,
            detail: v.detail,
            dispatched: v.dispatched,
        })
    }
}
impl From<&OrderedOpenCompletionV2> for MobileOrderedOpenCompletion {
    fn from(v: &OrderedOpenCompletionV2) -> Self {
        match v {
            OrderedOpenCompletionV2::Retired(r) => Self::Retired {
                slot_identity: r.slot_identity.into(),
            },
            OrderedOpenCompletionV2::Result(r) => Self::Result {
                slot_identity: r.slot_identity.into(),
                outcome: match &r.outcome {
                    OrderedOpenOutcomeV2::Pending => MobileOrderedOpenOutcome::Pending,
                    OrderedOpenOutcomeV2::Opened {
                        source_generation,
                        members,
                    } => MobileOrderedOpenOutcome::Opened {
                        source_generation: source_generation.into(),
                        members: members.iter().copied().map(Into::into).collect(),
                    },
                    OrderedOpenOutcomeV2::Failed {
                        original_failure,
                        ownership,
                    } => MobileOrderedOpenOutcome::Failed {
                        original_failure: original_failure.into(),
                        ownership: match ownership {
                            OrderedOpenOwnershipV2::NotCreated => {
                                MobileOrderedOwnershipV2::NotCreated
                            }
                            OrderedOpenOwnershipV2::Created {
                                source_generation,
                                members,
                            } => MobileOrderedOwnershipV2::Created {
                                source_generation: source_generation.into(),
                                members: members.iter().copied().map(Into::into).collect(),
                            },
                        },
                    },
                    OrderedOpenOutcomeV2::Indeterminate {
                        original_failure_or_reason,
                    } => MobileOrderedOpenOutcome::Indeterminate {
                        original_failure_or_reason: match original_failure_or_reason {
                            OrderedOpenIndeterminateReason::Failure(failure) => {
                                MobileOrderedIndeterminateReason::Failure {
                                    failure: failure.into(),
                                }
                            }
                            OrderedOpenIndeterminateReason::Reason(reason) => {
                                MobileOrderedIndeterminateReason::Reason {
                                    reason: reason.clone(),
                                }
                            }
                        },
                    },
                },
            },
        }
    }
}
impl TryFrom<MobileOrderedOpenCompletion> for OrderedOpenCompletionV2 {
    type Error = String;
    fn try_from(v: MobileOrderedOpenCompletion) -> Result<Self, String> {
        Ok(match v {
            MobileOrderedOpenCompletion::Retired { slot_identity } => {
                Self::Retired(OrderedOpenRetired {
                    slot_identity: slot_identity.try_into()?,
                })
            }
            MobileOrderedOpenCompletion::Result {
                slot_identity,
                outcome,
            } => Self::Result(OrderedOpenResultV2 {
                slot_identity: slot_identity.try_into()?,
                outcome: match outcome {
                    MobileOrderedOpenOutcome::Pending => OrderedOpenOutcomeV2::Pending,
                    MobileOrderedOpenOutcome::Opened {
                        source_generation,
                        members,
                    } => OrderedOpenOutcomeV2::Opened {
                        source_generation: ordered_generation(source_generation)?,
                        members: members.into_iter().map(Into::into).collect(),
                    },
                    MobileOrderedOpenOutcome::Failed {
                        original_failure,
                        ownership,
                    } => OrderedOpenOutcomeV2::Failed {
                        original_failure: original_failure.try_into()?,
                        ownership: match ownership {
                            MobileOrderedOwnershipV2::NotCreated => {
                                OrderedOpenOwnershipV2::NotCreated
                            }
                            MobileOrderedOwnershipV2::Created {
                                source_generation,
                                members,
                            } => OrderedOpenOwnershipV2::Created {
                                source_generation: ordered_generation(source_generation)?,
                                members: members.into_iter().map(Into::into).collect(),
                            },
                        },
                    },
                    MobileOrderedOpenOutcome::Indeterminate {
                        original_failure_or_reason,
                    } => OrderedOpenOutcomeV2::Indeterminate {
                        original_failure_or_reason: match original_failure_or_reason {
                            MobileOrderedIndeterminateReason::Failure { failure } => {
                                OrderedOpenIndeterminateReason::Failure(failure.try_into()?)
                            }
                            MobileOrderedIndeterminateReason::Reason { reason } => {
                                OrderedOpenIndeterminateReason::Reason(reason)
                            }
                        },
                    },
                },
            }),
        })
    }
}

impl From<&OrderedOpenRequestV2> for super::MobileRadioRequest {
    fn from(value: &OrderedOpenRequestV2) -> Self {
        match value {
            OrderedOpenRequestV2::Open { id, identity } => Self::OrderedOpenV2 {
                id: *id,
                identity: identity.into(),
            },
            OrderedOpenRequestV2::Recover { id, identity } => Self::OrderedRecoverOpenV2 {
                id: *id,
                identity: identity.into(),
            },
            OrderedOpenRequestV2::Acknowledge { id, identity } => Self::OrderedAcknowledgeOpenV2 {
                id: *id,
                identity: identity.into(),
            },
        }
    }
}

impl TryFrom<super::MobileRadioCompletion> for OrderedOpenCompletionV2 {
    type Error = String;
    fn try_from(value: super::MobileRadioCompletion) -> Result<Self, String> {
        match value {
            super::MobileRadioCompletion::OrderedOpenResultV2 {
                slot_identity,
                outcome,
            } => MobileOrderedOpenCompletion::Result {
                slot_identity,
                outcome,
            }
            .try_into(),
            super::MobileRadioCompletion::OrderedOpenRetiredV2 { slot_identity } => {
                MobileOrderedOpenCompletion::Retired { slot_identity }.try_into()
            }
            _ => Err("not an ordered admission v2 completion".into()),
        }
    }
}

impl TryFrom<super::MobileRadioRequest> for OrderedOpenRequestV2 {
    type Error = String;
    fn try_from(value: super::MobileRadioRequest) -> Result<Self, String> {
        let (id, identity, kind) = match value {
            super::MobileRadioRequest::OrderedOpenV2 { id, identity } => (id, identity, 0),
            super::MobileRadioRequest::OrderedRecoverOpenV2 { id, identity } => (id, identity, 1),
            super::MobileRadioRequest::OrderedAcknowledgeOpenV2 { id, identity } => {
                (id, identity, 2)
            }
            _ => return Err("not an ordered admission v2 request".into()),
        };
        let identity = OrderedOpenIdentity::try_from(identity)?;
        if (kind == 0) != (id == identity.original_request_id) {
            return Err("invalid ordered open attempt id".into());
        }
        Ok(match kind {
            0 => Self::Open { id, identity },
            1 => Self::Recover { id, identity },
            _ => Self::Acknowledge { id, identity },
        })
    }
}

/// Decode only a response that answers this exact v2 attempt and slot.
pub fn checked_ordered_open_completion(
    value: super::MobileRadioCompletion,
    request: &OrderedOpenRequestV2,
) -> Result<OrderedOpenCompletionV2, String> {
    let completion = OrderedOpenCompletionV2::try_from(value)?;
    if !completion.answers(request) {
        return Err("ordered admission response does not answer the request".into());
    }
    Ok(completion)
}

/// Negotiate v2 only. The v1 host installation must switch to this helper
/// together with its v2 request and completion caller seam.
pub fn negotiate_ordered_admission_v2(
    radio: &dyn super::MobilePlatformRadio,
) -> Result<OrderedAdmissionOwner, super::MobileCoreError> {
    let response = radio.ordered_admission_owner(ubm_mobile::ORDERED_ADMISSION_SLOT_VERSION);
    OrderedAdmissionOwner::negotiate_slot(ubm_mobile::ORDERED_ADMISSION_SLOT_VERSION, &response)
        .map_err(|detail| super::MobileCoreError::Failed {
            code: if response.is_empty() {
                "protocol.incompatible"
            } else {
                "protocol.malformed"
            }
            .into(),
            domain: "boundary".into(),
            operation: "mobile.host.install".into(),
            detail: Some(detail.into()),
        })
}

impl From<&OrderedOpenCompletionV2> for super::MobileRadioCompletion {
    fn from(value: &OrderedOpenCompletionV2) -> Self {
        match MobileOrderedOpenCompletion::from(value) {
            MobileOrderedOpenCompletion::Result {
                slot_identity,
                outcome,
            } => Self::OrderedOpenResultV2 {
                slot_identity,
                outcome,
            },
            MobileOrderedOpenCompletion::Retired { slot_identity } => {
                Self::OrderedOpenRetiredV2 { slot_identity }
            }
        }
    }
}
