//! Read-only IPC directory projection. No connection leases or scan observations
//! are created by a lookup; the OS remains the identity authority.
use super::*;

const BACKEND: &str = "unified-ble:corebluetooth";

fn directory_backend(os: ubm_desktop::DesktopOs) -> &'static str {
    match os {
        ubm_desktop::DesktopOs::Windows => "unified-ble:winrt",
        ubm_desktop::DesktopOs::Linux => "unified-ble:bluez-dbus",
        ubm_desktop::DesktopOs::MacOs => BACKEND,
    }
}

#[cfg(test)]
mod reference_tests {
    use super::*;

    #[test]
    fn directory_reference_scope_matches_the_requested_native_route() {
        for (backend, id) in [
            (BACKEND, "00112233-4455-6677-8899-aabbccddeeff"),
            ("unified-ble:winrt", "AA:BB:CC:DD:EE:FF"),
            ("unified-ble:bluez-dbus", "hci0/dev_AA_BB_CC_DD_EE_FF"),
        ] {
            let value = object([
                ("version", IpcValue::Number(1.into())),
                ("backendId", string(backend)),
                ("scope", string("application")),
                ("opaqueId", string(id)),
            ]);
            assert_eq!(reference(&value, backend).unwrap(), id);
            for foreign in [BACKEND, "unified-ble:winrt", "unified-ble:bluez-dbus"] {
                if foreign != backend {
                    assert_eq!(
                        reference(&value, foreign).unwrap_err().code,
                        BleErrorCode::PeerScopeMismatch
                    );
                }
            }
        }
    }
}

fn native_identifier(value: &str, backend: &str, op: &str) -> Result<String, DispatchError> {
    match backend {
        "unified-ble:winrt" => {
            let (kind, address) = match value.split_once(':') {
                Some((kind @ ("public" | "random" | "unknown"), address)) => (Some(kind), address),
                _ => (None, value),
            };
            let parts = address.split(':').collect::<Vec<_>>();
            if parts.len() != 6
                || parts.iter().any(|part| {
                    part.len() != 2 || !part.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
            {
                return Err(malformed(op));
            }
            Ok(kind
                .map(|kind| format!("{kind}:{}", address.to_uppercase()))
                .unwrap_or_else(|| address.to_uppercase()))
        }
        "unified-ble:bluez-dbus" => {
            let (adapter, device) = value.split_once('/').ok_or_else(|| malformed(op))?;
            let index = adapter.strip_prefix("hci").ok_or_else(|| malformed(op))?;
            let bytes = device
                .strip_prefix("dev_")
                .ok_or_else(|| malformed(op))?
                .split('_')
                .collect::<Vec<_>>();
            if index.is_empty()
                || !index.bytes().all(|byte| byte.is_ascii_digit())
                || bytes.len() != 6
                || bytes.iter().any(|part| {
                    part.len() != 2
                        || !part
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'A'..=b'F').contains(&byte))
                })
            {
                return Err(malformed(op));
            }
            Ok(value.to_owned())
        }
        BACKEND => uuid(&value.to_lowercase(), op),
        _ => Err(malformed(op)),
    }
}

fn malformed(operation: &str) -> DispatchError {
    DispatchError::new(BleErrorCode::ProtocolMalformed, "ipc", operation)
}

fn unsupported(operation: &str) -> DispatchError {
    DispatchError::new(BleErrorCode::CapabilityUnsupported, "connection", operation)
        .platform("The selected directory mechanism cannot answer this query")
}

fn admit(ctl: &OpControl, operation: &str) -> Result<(), DispatchError> {
    if ctl.ticket.is_cancel_requested() || ctl.ticket.is_reset() {
        return Err(DispatchError::from_core(
            &ctl.ticket.interruption(operation),
        ));
    }
    if ctl
        .budget
        .remaining()
        .is_some_and(|remaining| remaining.is_zero())
    {
        return Err(DispatchError::new(
            BleErrorCode::OperationTimedOut,
            "connection",
            operation,
        ));
    }
    Ok(())
}

fn keys(
    value: &BTreeMap<String, IpcValue>,
    allowed: &[&str],
    op: &str,
) -> Result<(), DispatchError> {
    if value.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(malformed(op));
    }
    Ok(())
}

fn uuid(value: &str, op: &str) -> Result<String, DispatchError> {
    let parsed = Uuid::parse_str(value).map_err(|_| malformed(op))?;
    let canonical = parsed.hyphenated().to_string();
    if value != canonical {
        return Err(malformed(op));
    }
    Ok(canonical)
}

fn reference(value: &IpcValue, expected_backend: &str) -> Result<String, DispatchError> {
    decode_reference(value, expected_backend).map_err(|error| {
        if error.code == BleErrorCode::ProtocolMalformed {
            DispatchError::new(
                BleErrorCode::PeerReferenceInvalid,
                "connection",
                "peers.reference",
            )
        } else {
            error
        }
    })
}

fn decode_reference(value: &IpcValue, expected_backend: &str) -> Result<String, DispatchError> {
    let op = "peers.reference";
    let record = into_object(value.clone(), op)?;
    keys(&record, &["version", "backendId", "scope", "opaqueId"], op)?;
    if record.get("version") != Some(&IpcValue::Number(Number::from(1))) {
        return Err(DispatchError::new(
            BleErrorCode::PeerReferenceInvalid,
            "connection",
            op,
        ));
    }
    let opaque = required_string(&record, "opaqueId", op)?;
    let backend = required_string(&record, "backendId", op)?;
    if backend != expected_backend || required_string(&record, "scope", op)? != "application" {
        return Err(DispatchError::new(
            BleErrorCode::PeerScopeMismatch,
            "connection",
            op,
        ));
    }
    let canonical = native_identifier(&opaque, &backend, op)
        .map_err(|_| DispatchError::new(BleErrorCode::PeerReferenceInvalid, "connection", op))?;
    Ok(canonical)
}

fn strings(value: Option<&IpcValue>, op: &str) -> Result<Option<Vec<String>>, DispatchError> {
    value
        .map(|value| match value {
            IpcValue::Array(values) => values
                .iter()
                .map(|item| match item {
                    IpcValue::String(value) => Ok(value.clone()),
                    _ => Err(malformed(op)),
                })
                .collect(),
            _ => Err(malformed(op)),
        })
        .transpose()
}

fn record(
    peer: ubm_desktop::DirectoryPeer,
    source: &str,
    backend: &str,
) -> Result<IpcValue, DispatchError> {
    let id = native_identifier(&peer.peer_id, backend, "peers.record")?;
    if !matches!(peer.connection, "connected" | "disconnected" | "unknown") {
        return Err(malformed("peers.record"));
    }
    Ok(object([
        (
            "reference",
            object([
                ("version", IpcValue::Number(Number::from(1))),
                ("backendId", string(backend)),
                ("scope", string("application")),
                ("opaqueId", string(&id)),
            ]),
        ),
        ("peerId", string(id)),
        ("name", peer.name.map(string).unwrap_or(IpcValue::Null)),
        ("rssi", IpcValue::Null),
        ("source", string(source)),
        (
            "state",
            object([
                ("reachability", string("unknown")),
                ("connection", string(peer.connection)),
                (
                    "bond",
                    string(if source == "system-bonded" {
                        "bonded"
                    } else if backend == BACKEND {
                        "unsupported"
                    } else {
                        "unknown"
                    }),
                ),
                ("lastSeenAtMonotonicMs", IpcValue::Null),
            ]),
        ),
    ]))
}

impl BtleplugDispatcher {
    pub(super) async fn peer_directory(
        &self,
        caller: &AuthenticatedCaller,
        command: &str,
        attachment: &Attachment,
        payload: BTreeMap<String, IpcValue>,
        ctl: OpControl,
    ) -> Result<IpcValue, DispatchError> {
        keys(
            &payload,
            &[
                "reference",
                "query",
                "deadline",
                "budgetMs",
                "__expectedLeaseId",
                "__expectedLeaseGeneration",
            ],
            command,
        )?;
        if payload.get("deadline").is_some_and(|value| !matches!(value,IpcValue::Null) && !matches!(value,IpcValue::Number(number) if number.as_f64().is_some_and(|value| value.is_finite() && value >= 0.0 && value <= MAX_SAFE_INTEGER as f64))) { return Err(malformed(command)); }
        let resolving = command == "peers.resolve";
        let mut reference_values = if resolving {
            Some(vec![required_value(&payload, "reference", command)?.clone()])
        } else {
            None
        };
        let query = if resolving {
            if payload.contains_key("query") {
                return Err(malformed(command));
            }
            BTreeMap::new()
        } else {
            if payload.contains_key("reference") {
                return Err(malformed(command));
            }
            into_object(required_value(&payload, "query", command)?.clone(), command)?
        };
        keys(
            &query,
            &["sources", "services", "references", "includeUnavailable"],
            command,
        )?;
        if query
            .get("includeUnavailable")
            .is_some_and(|v| !matches!(v, IpcValue::Bool(_)))
        {
            return Err(malformed(command));
        }
        let sources = strings(query.get("sources"), command)?;
        if sources.as_ref().is_some_and(|sources| {
            sources.iter().any(|source| {
                ![
                    "scan-observed",
                    "app-reference",
                    "system-connected",
                    "system-bonded",
                    "origin-authorized",
                    "restored",
                    "backend-cache",
                ]
                .contains(&source.as_str())
            })
        }) {
            return Err(malformed(command));
        }
        let services = strings(query.get("services"), command)?
            .unwrap_or_default()
            .iter()
            .map(|s| uuid(s, command))
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(value) = query.get("references") {
            let IpcValue::Array(values) = value else {
                return Err(malformed(command));
            };
            reference_values = Some(values.clone());
        }
        let authority = self.ensure_authority().await?;
        let states = authority
            .capability_descriptors()
            .await
            .map_err(|error| DispatchError::from_core(&error))?;
        let capability = match command {
            "peers.resolve" => capabilities::reference_resolution_mechanism(&states),
            "peers.known" => "peer:known",
            "peers.connected" => "peer:system-connected",
            "peers.bonded" => "peer:bonded",
            _ => return Err(unsupported(command)),
        };
        let resolving_bonded = resolving && capability == "peer:bonded";
        let reference_backend = directory_backend(authority.directory_os());
        let apple_directory = reference_backend == BACKEND;
        let references = reference_values
            .map(|values| {
                values
                    .iter()
                    .map(|value| reference(value, reference_backend))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?;
        match states
            .iter()
            .find(|row| row.id() == capability)
            .map(|row| row.state())
        {
            Some(
                ubm_core::central::CapabilityState::Supported
                | ubm_core::central::CapabilityState::Limited,
            ) => {}
            Some(ubm_core::central::CapabilityState::Unavailable) => {
                return Err(DispatchError::new(
                    BleErrorCode::CapabilityUnavailable,
                    "connection",
                    command,
                ));
            }
            _ => return Err(unsupported(command)),
        }
        match command {
            "peers.connected" if apple_directory && services.is_empty() => {
                return Err(unsupported("peers.connected.services-required"));
            }
            "peers.known" if apple_directory && references.is_none() => {
                return Err(unsupported("peers.known.references-required"));
            }
            "peers.connected" if !apple_directory && !services.is_empty() => {
                return Err(unsupported("peers.connected.services"));
            }
            "peers.known" if !services.is_empty() => {
                return Err(unsupported("peers.known.services"));
            }
            "peers.bonded" if !services.is_empty() => {
                return Err(unsupported("peers.bonded.services"));
            }
            "peers.resolve" | "peers.connected" | "peers.known" | "peers.bonded" => {}
            _ => return Err(unsupported(command)),
        }
        admit(&ctl, command)?;
        let source = if command == "peers.connected" {
            "system-connected"
        } else if command == "peers.bonded" || resolving_bonded {
            "system-bonded"
        } else if command == "peers.known" && !apple_directory {
            "backend-cache"
        } else {
            "app-reference"
        };
        let mut records = Vec::new();
        let mut seen = HashSet::new();
        {
            if command == "peers.connected" {
                let peers = authority
                    .connected_peers(&services, ctl.clone())
                    .await
                    .map_err(|error| DispatchError::from_core(&error))?;
                for peer in peers {
                    if peer.connection != "connected" {
                        return Err(malformed("peers.connected.record"));
                    }
                    let id = native_identifier(&peer.peer_id, reference_backend, "peers.record")?;
                    if references.as_ref().is_none_or(|refs| refs.contains(&id)) && seen.insert(id)
                    {
                        records.push(record(peer, source, reference_backend)?);
                    }
                }
            } else if command == "peers.known" && !apple_directory {
                let peers = authority
                    .known_peers(ctl.clone())
                    .await
                    .map_err(|error| DispatchError::from_core(&error))?;
                for peer in peers {
                    let id = native_identifier(&peer.peer_id, reference_backend, "peers.record")?;
                    if references.as_ref().is_none_or(|refs| refs.contains(&id)) && seen.insert(id)
                    {
                        records.push(record(peer, source, reference_backend)?);
                    }
                }
            } else if command == "peers.bonded" || resolving_bonded {
                let peers = authority
                    .bonded_peers(ctl.clone())
                    .await
                    .map_err(|error| DispatchError::from_core(&error))?;
                for peer in peers {
                    let id = native_identifier(&peer.peer_id, reference_backend, "peers.record")?;
                    if references.as_ref().is_none_or(|refs| refs.contains(&id)) && seen.insert(id)
                    {
                        records.push(record(peer, source, reference_backend)?);
                    }
                }
            } else {
                for id in references.unwrap_or_default() {
                    if !seen.insert(id.clone()) {
                        continue;
                    }
                    self.refuse_stale_attachment(attachment, command).await?;
                    let child = OpControl::new(ctl.budget, OpTicket::new());
                    let resolved = tokio::select! {
                        biased;
                        _ = ctl.ticket.cancelled() => return Err(DispatchError::from_core(&ctl.ticket.interruption(command))),
                        result = authority.resolve_peer(&id, child) => result.map_err(|error| DispatchError::from_core(&error))?,
                    };
                    if let Some(peer) = resolved {
                        if peer.peer_id != id {
                            return Err(malformed("peers.resolve.identity"));
                        }
                        records.push(record(peer, source, reference_backend)?);
                    }
                }
            }
        }
        if sources
            .as_ref()
            .is_some_and(|sources| !sources.iter().any(|s| s == source))
        {
            records.clear();
        }
        // One batch cannot publish mixed adapter generations or outlive its caller.
        self.refuse_stale_attachment(attachment, command).await?;
        self.validate_expected_lease(caller, &payload).await?;
        admit(&ctl, command)?;
        if resolving {
            Ok(object([(
                "peer",
                records.into_iter().next().unwrap_or(IpcValue::Null),
            )]))
        } else {
            Ok(object([("peers", IpcValue::Array(records))]))
        }
    }
}
