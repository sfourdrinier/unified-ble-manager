use std::collections::BTreeMap;

use serde_json::Number;
use uuid::Uuid;

use crate::wire::IpcValue;

const MAX_QUERY_CLAUSES: usize = 64;
const MAX_QUERY_PREDICATES: usize = 64;
const MAX_DIAGNOSTIC_LIMITATIONS: usize = 32;

#[derive(Clone, Debug)]
pub(crate) struct DecodedScanQuery {
    pub(crate) wire: IpcValue,
    pub(crate) digest: String,
    predicates: Vec<Predicate>,
    native_predicates: Vec<Predicate>,
    unavailable: Vec<Predicate>,
    pub(crate) native_service_uuids: Vec<String>,
}

#[derive(Clone, Debug)]
struct Clause {
    peers: Option<Vec<PeerReference>>,
    services: Option<UuidField>,
    names: Option<NameField>,
    manufacturer_data: Option<DataField<ManufacturerPattern>>,
    service_data: Option<DataField<ServicePattern>>,
    rssi: Option<RssiField>,
    connectable: Option<bool>,
}

#[derive(Clone, Debug)]
struct PeerReference {
    backend_id: String,
    scope: String,
    opaque_id: String,
}

#[derive(Clone, Debug)]
struct UuidField {
    any: Vec<String>,
    all: Vec<String>,
}

#[derive(Clone, Debug)]
struct NameField {
    exact: Vec<String>,
    prefixes: Vec<String>,
}

#[derive(Clone, Debug)]
struct DataField<Pattern> {
    any: Vec<Pattern>,
    all: Vec<Pattern>,
}

#[derive(Clone, Debug)]
struct ManufacturerPattern {
    company_id: u16,
    data_prefix: Option<Vec<u8>>,
    mask: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
struct ServicePattern {
    service: String,
    data_prefix: Option<Vec<u8>>,
    mask: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
struct RssiField {
    minimum: Option<Number>,
    maximum: Option<Number>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Predicate {
    clause_set: &'static str,
    clause_index: usize,
    field: &'static str,
    operator: &'static str,
}

pub(crate) fn decode_normalized_scan_query(value: &IpcValue) -> Result<DecodedScanQuery, String> {
    let query = object_ref(value, "normalized scan query")?;
    exact_keys(
        query,
        &["anyOf", "exclude", "digest"],
        &[],
        "normalized scan query",
    )?;
    let any_of = parse_clause_list(query, "anyOf")?;
    let exclude = parse_clause_list(query, "exclude")?;
    let supplied_digest = string_field(query, "digest", "normalized scan query digest")?;
    let canonical = canonical_query_json(any_of.as_deref(), exclude.as_deref());
    let digest = scan_query_digest(&canonical);
    if supplied_digest != digest {
        return Err("normalized scan query digest is invalid".to_owned());
    }
    let wire = query_wire(any_of.as_deref(), exclude.as_deref(), &digest);
    let predicates = describe_predicates(any_of.as_deref(), exclude.as_deref());
    if predicates.len() > MAX_QUERY_PREDICATES {
        return Err("normalized scan query exceeds the bounded predicate count".to_owned());
    }
    let native_service_uuids = common_required_services(any_of.as_deref());
    let native_predicates = predicates
        .iter()
        .filter(|predicate| {
            is_fully_pushed_service_predicate(predicate, any_of.as_deref(), &native_service_uuids)
        })
        .cloned()
        .collect::<Vec<_>>();
    let unavailable = predicates
        .iter()
        .filter(|predicate| !observation_field_available(predicate.field))
        .cloned()
        .collect::<Vec<_>>();

    Ok(DecodedScanQuery {
        wire,
        digest,
        predicates,
        native_predicates,
        unavailable,
        native_service_uuids,
    })
}

pub(crate) fn diagnostic_scan_plan(query: &DecodedScanQuery) -> IpcValue {
    let limitations = query
        .predicates
        .iter()
        .take(MAX_DIAGNOSTIC_LIMITATIONS)
        .map(|predicate| {
            let (code, explanation, effect) = if query.unavailable.contains(predicate) {
                (
                    "observation-field-unavailable",
                    "required observation field is unavailable on this host",
                    "field-unavailable",
                )
            } else if query.native_predicates.contains(predicate) {
                (
                    "native-filter-incomplete",
                    "predicate remains in the canonical residual matcher",
                    "performance-only",
                )
            } else {
                (
                    "host-predicate-restricted",
                    "host restriction prevents native evaluation",
                    "host-restriction",
                )
            };
            ipc_object([
                ("code", string(code)),
                ("predicate", predicate_wire(predicate)),
                ("explanation", string(explanation)),
                ("effect", string(effect)),
            ])
        })
        .collect::<Vec<_>>();
    let native_predicates = query
        .native_predicates
        .iter()
        .map(predicate_wire)
        .collect::<Vec<_>>();
    let predicates = query
        .predicates
        .iter()
        .map(predicate_wire)
        .collect::<Vec<_>>();
    let unavailable = query
        .unavailable
        .iter()
        .map(predicate_wire)
        .collect::<Vec<_>>();
    let estimated_cost = if query.native_service_uuids.is_empty() {
        "high"
    } else {
        "moderate"
    };

    ipc_object([
        ("sourceQuery", query.wire.clone()),
        ("queryDigest", string(query.digest.as_str())),
        ("residualQueryDigest", string(query.digest.as_str())),
        ("nativeGuarantee", string("safe-superset")),
        (
            "native",
            ipc_object([
                ("predicates", IpcValue::Array(native_predicates)),
                ("complete", IpcValue::Bool(false)),
            ]),
        ),
        (
            "residual",
            ipc_object([
                ("query", query.wire.clone()),
                ("predicates", IpcValue::Array(predicates)),
                ("complete", IpcValue::Bool(true)),
            ]),
        ),
        ("unavailable", IpcValue::Array(unavailable)),
        ("limitations", IpcValue::Array(limitations)),
        ("estimatedCost", string(estimated_cost)),
    ])
}

fn parse_clause_list(
    query: &BTreeMap<String, IpcValue>,
    key: &str,
) -> Result<Option<Vec<Clause>>, String> {
    match query.get(key) {
        Some(IpcValue::Null) => Ok(None),
        Some(IpcValue::Array(values)) if !values.is_empty() => {
            if values.len() > MAX_QUERY_CLAUSES {
                return Err(format!(
                    "normalized scan query {key} exceeds the clause count"
                ));
            }
            let mut clauses = values
                .iter()
                .map(parse_clause)
                .collect::<Result<Vec<_>, _>>()?;
            clauses.sort_by_key(clause_canonical_json);
            Ok(Some(clauses))
        }
        _ => Err(format!(
            "normalized scan query {key} must be null or non-empty"
        )),
    }
}

fn parse_clause(value: &IpcValue) -> Result<Clause, String> {
    let clause = object_ref(value, "normalized scan clause")?;
    exact_keys(
        clause,
        &[
            "peers",
            "services",
            "names",
            "manufacturerData",
            "serviceData",
            "rssi",
        ],
        &["connectable"],
        "normalized scan clause",
    )?;
    let peers = parse_peers(clause.get("peers"))?;
    let services = parse_uuid_field(clause.get("services"), "services")?;
    let names = parse_name_field(clause.get("names"))?;
    let manufacturer_data = parse_data_field(clause.get("manufacturerData"), false)?;
    let service_data = parse_data_field(clause.get("serviceData"), true)?;
    let rssi = parse_rssi(clause.get("rssi"))?;
    let connectable = match clause.get("connectable") {
        None => None,
        Some(IpcValue::Null) => None,
        Some(IpcValue::Bool(value)) => Some(*value),
        Some(_) => return Err("normalized scan clause connectable is invalid".to_owned()),
    };
    if peers.is_none()
        && services.is_none()
        && names.is_none()
        && manufacturer_data.is_none()
        && service_data.is_none()
        && rssi.is_none()
        && connectable.is_none()
    {
        return Err("normalized scan clause must not be empty".to_owned());
    }
    Ok(Clause {
        peers,
        services,
        names,
        manufacturer_data,
        service_data,
        rssi,
        connectable,
    })
}

fn parse_peers(value: Option<&IpcValue>) -> Result<Option<Vec<PeerReference>>, String> {
    match value {
        Some(IpcValue::Null) => Ok(None),
        Some(IpcValue::Array(values)) => {
            let mut peers = values
                .iter()
                .map(parse_peer)
                .collect::<Result<Vec<_>, _>>()?;
            peers.sort_by_key(peer_canonical_json);
            peers.dedup_by(|left, right| peer_canonical_json(left) == peer_canonical_json(right));
            Ok(Some(peers))
        }
        _ => Err("normalized scan clause peers is invalid".to_owned()),
    }
}

fn parse_peer(value: &IpcValue) -> Result<PeerReference, String> {
    let peer = object_ref(value, "normalized scan peer")?;
    exact_keys(
        peer,
        &["version", "backendId", "scope", "opaqueId"],
        &[],
        "normalized scan peer",
    )?;
    if number_field(peer, "version", "normalized scan peer")?.as_u64() != Some(1) {
        return Err("normalized scan peer version is invalid".to_owned());
    }
    let backend_id = string_field(peer, "backendId", "normalized scan peer")?;
    let scope = string_field(peer, "scope", "normalized scan peer")?;
    if !matches!(scope.as_str(), "application" | "origin" | "system") {
        return Err("normalized scan peer scope is invalid".to_owned());
    }
    let opaque_id = string_field(peer, "opaqueId", "normalized scan peer")?;
    Ok(PeerReference {
        backend_id,
        scope,
        opaque_id,
    })
}

fn parse_uuid_field(value: Option<&IpcValue>, field: &str) -> Result<Option<UuidField>, String> {
    let Some(value) = value else {
        return Err(format!("normalized scan clause {field} is missing"));
    };
    if matches!(value, IpcValue::Null) {
        return Ok(None);
    }
    let object = object_ref(value, field)?;
    exact_keys(object, &["any", "all"], &[], field)?;
    let any = parse_uuid_list(object.get("any"), field)?;
    let all = parse_uuid_list(object.get("all"), field)?;
    if any.is_empty() && all.is_empty() {
        return Err(format!("normalized scan clause {field} is empty"));
    }
    Ok(Some(UuidField { any, all }))
}

fn parse_uuid_list(value: Option<&IpcValue>, field: &str) -> Result<Vec<String>, String> {
    let Some(IpcValue::Array(values)) = value else {
        return Err(format!("normalized scan clause {field} list is invalid"));
    };
    let mut result = values
        .iter()
        .map(|value| match value {
            IpcValue::String(value) if !value.is_empty() => canonical_uuid(value)
                .ok_or_else(|| format!("normalized scan clause {field} UUID is invalid")),
            _ => Err(format!("normalized scan clause {field} UUID is invalid")),
        })
        .collect::<Result<Vec<_>, _>>()?;
    result.sort();
    result.dedup();
    Ok(result)
}

fn parse_name_field(value: Option<&IpcValue>) -> Result<Option<NameField>, String> {
    let Some(value) = value else {
        return Err("normalized scan clause names is missing".to_owned());
    };
    if matches!(value, IpcValue::Null) {
        return Ok(None);
    }
    let field = object_ref(value, "normalized scan clause names")?;
    exact_keys(
        field,
        &["exact", "prefixes"],
        &[],
        "normalized scan clause names",
    )?;
    let exact = parse_non_empty_strings(field.get("exact"), "normalized scan names exact")?;
    let prefixes =
        parse_non_empty_strings(field.get("prefixes"), "normalized scan names prefixes")?;
    if exact.is_empty() && prefixes.is_empty() {
        return Err("normalized scan clause names is empty".to_owned());
    }
    Ok(Some(NameField { exact, prefixes }))
}

fn parse_non_empty_strings(
    value: Option<&IpcValue>,
    operation: &str,
) -> Result<Vec<String>, String> {
    let Some(IpcValue::Array(values)) = value else {
        return Err(format!("{operation} is invalid"));
    };
    let mut result = values
        .iter()
        .map(|value| match value {
            IpcValue::String(value) if !value.is_empty() => Ok(value.clone()),
            _ => Err(format!("{operation} contains an invalid string")),
        })
        .collect::<Result<Vec<_>, _>>()?;
    result.sort();
    result.dedup();
    Ok(result)
}

fn parse_data_field<Pattern>(
    value: Option<&IpcValue>,
    service_data: bool,
) -> Result<Option<DataField<Pattern>>, String>
where
    Pattern: ParsePattern,
{
    let Some(value) = value else {
        return Err("normalized scan data field is missing".to_owned());
    };
    if matches!(value, IpcValue::Null) {
        return Ok(None);
    }
    let field_name = if service_data {
        "serviceData"
    } else {
        "manufacturerData"
    };
    let field = object_ref(value, field_name)?;
    exact_keys(field, &["any", "all"], &[], field_name)?;
    let any = parse_patterns(field.get("any"), field_name)?;
    let all = parse_patterns(field.get("all"), field_name)?;
    if any.is_empty() && all.is_empty() {
        return Err(format!("normalized scan clause {field_name} is empty"));
    }
    Ok(Some(DataField { any, all }))
}

trait ParsePattern: Sized {
    fn parse(value: &IpcValue) -> Result<Self, String>;
    fn canonical_json(&self) -> String;
}

impl ParsePattern for ManufacturerPattern {
    fn parse(value: &IpcValue) -> Result<Self, String> {
        let pattern = object_ref(value, "normalized manufacturer pattern")?;
        exact_keys(
            pattern,
            &["companyId"],
            &["dataPrefix", "mask"],
            "normalized manufacturer pattern",
        )?;
        let company_id = number_field(pattern, "companyId", "normalized manufacturer pattern")?
            .as_u64()
            .and_then(|value| u16::try_from(value).ok())
            .ok_or_else(|| "normalized manufacturer companyId is invalid".to_owned())?;
        let (data_prefix, mask) = parse_bytes_and_mask(pattern, "normalized manufacturer pattern")?;
        Ok(Self {
            company_id,
            data_prefix,
            mask,
        })
    }

    fn canonical_json(&self) -> String {
        let mut output = format!("{{\"companyId\":{}", self.company_id);
        if let Some(prefix) = &self.data_prefix {
            output.push_str(",\"dataPrefix\":");
            output.push_str(&json_string(&bytes_to_hex(prefix)));
        }
        if let Some(mask) = &self.mask {
            output.push_str(",\"mask\":");
            output.push_str(&json_string(&bytes_to_hex(mask)));
        }
        output.push('}');
        output
    }
}

impl ParsePattern for ServicePattern {
    fn parse(value: &IpcValue) -> Result<Self, String> {
        let pattern = object_ref(value, "normalized service pattern")?;
        exact_keys(
            pattern,
            &["service"],
            &["dataPrefix", "mask"],
            "normalized service pattern",
        )?;
        let service = string_field(pattern, "service", "normalized service pattern")?;
        let service = canonical_uuid(&service)
            .ok_or_else(|| "normalized service UUID is invalid".to_owned())?;
        let (data_prefix, mask) = parse_bytes_and_mask(pattern, "normalized service pattern")?;
        Ok(Self {
            service,
            data_prefix,
            mask,
        })
    }

    fn canonical_json(&self) -> String {
        let mut output = format!("{{\"service\":{}", json_string(&self.service));
        if let Some(prefix) = &self.data_prefix {
            output.push_str(",\"dataPrefix\":");
            output.push_str(&json_string(&bytes_to_hex(prefix)));
        }
        if let Some(mask) = &self.mask {
            output.push_str(",\"mask\":");
            output.push_str(&json_string(&bytes_to_hex(mask)));
        }
        output.push('}');
        output
    }
}

fn parse_patterns<Pattern: ParsePattern>(
    value: Option<&IpcValue>,
    field: &str,
) -> Result<Vec<Pattern>, String> {
    let Some(IpcValue::Array(values)) = value else {
        return Err(format!("normalized scan {field} patterns are invalid"));
    };
    let mut patterns = values
        .iter()
        .map(Pattern::parse)
        .collect::<Result<Vec<_>, _>>()?;
    patterns.sort_by_key(ParsePattern::canonical_json);
    Ok(patterns)
}

fn parse_bytes_and_mask(
    pattern: &BTreeMap<String, IpcValue>,
    operation: &str,
) -> Result<(Option<Vec<u8>>, Option<Vec<u8>>), String> {
    let data_prefix = match pattern.get("dataPrefix") {
        None => None,
        Some(IpcValue::Bytes(value)) if !value.is_empty() => Some(value.clone()),
        _ => return Err(format!("{operation} dataPrefix is invalid")),
    };
    let mask = match pattern.get("mask") {
        None => None,
        Some(IpcValue::Bytes(value))
            if data_prefix
                .as_ref()
                .is_some_and(|prefix| prefix.len() == value.len()) =>
        {
            Some(value.clone())
        }
        _ => return Err(format!("{operation} mask is invalid")),
    };
    Ok((data_prefix, mask))
}

fn parse_rssi(value: Option<&IpcValue>) -> Result<Option<RssiField>, String> {
    let Some(value) = value else {
        return Err("normalized scan clause rssi is missing".to_owned());
    };
    if matches!(value, IpcValue::Null) {
        return Ok(None);
    }
    let field = object_ref(value, "normalized scan rssi")?;
    exact_keys(field, &[], &["minimum", "maximum"], "normalized scan rssi")?;
    let minimum = optional_number(field.get("minimum"), "normalized scan rssi minimum")?;
    let maximum = optional_number(field.get("maximum"), "normalized scan rssi maximum")?;
    if minimum.is_none() && maximum.is_none() {
        return Err("normalized scan rssi is empty".to_owned());
    }
    if let (Some(minimum), Some(maximum)) = (
        minimum.as_ref().and_then(|value| value.as_f64()),
        maximum.as_ref().and_then(|value| value.as_f64()),
    ) {
        if minimum > maximum {
            return Err("normalized scan rssi range is invalid".to_owned());
        }
    }
    Ok(Some(RssiField { minimum, maximum }))
}

fn optional_number(value: Option<&IpcValue>, operation: &str) -> Result<Option<Number>, String> {
    match value {
        None => Ok(None),
        Some(IpcValue::Number(value)) if value.as_f64().is_some_and(f64::is_finite) => {
            Ok(Some(value.clone()))
        }
        _ => Err(format!("{operation} is invalid")),
    }
}

fn common_required_services(any_of: Option<&[Clause]>) -> Vec<String> {
    let Some(any_of) = any_of else {
        return Vec::new();
    };
    let Some(first_services) = any_of.first().and_then(|clause| clause.services.as_ref()) else {
        return Vec::new();
    };
    if first_services.all.is_empty()
        || any_of.iter().any(|clause| {
            clause
                .services
                .as_ref()
                .map_or(true, |services| services.all.is_empty())
        })
    {
        return Vec::new();
    }
    first_services
        .all
        .iter()
        .filter(|service| {
            any_of.iter().all(|clause| {
                clause
                    .services
                    .as_ref()
                    .is_some_and(|services| services.all.contains(service))
            })
        })
        .cloned()
        .collect()
}

fn is_fully_pushed_service_predicate(
    predicate: &Predicate,
    any_of: Option<&[Clause]>,
    native_service_uuids: &[String],
) -> bool {
    if predicate.clause_set != "anyOf"
        || predicate.field != "services"
        || predicate.operator != "all"
    {
        return false;
    }
    let Some(clause) = any_of.and_then(|clauses| clauses.get(predicate.clause_index)) else {
        return false;
    };
    let Some(services) = clause.services.as_ref() else {
        return false;
    };
    services.all.len() == native_service_uuids.len()
        && services
            .all
            .iter()
            .all(|service| native_service_uuids.contains(service))
}

fn describe_predicates(any_of: Option<&[Clause]>, exclude: Option<&[Clause]>) -> Vec<Predicate> {
    let mut predicates = Vec::new();
    describe_clause_set(&mut predicates, "anyOf", any_of);
    describe_clause_set(&mut predicates, "exclude", exclude);
    predicates.sort_by_key(predicate_canonical_json);
    predicates
}

fn describe_clause_set(
    predicates: &mut Vec<Predicate>,
    clause_set: &'static str,
    clauses: Option<&[Clause]>,
) {
    let Some(clauses) = clauses else {
        return;
    };
    for (clause_index, clause) in clauses.iter().enumerate() {
        if clause.peers.is_some() {
            predicates.push(Predicate {
                clause_set,
                clause_index,
                field: "peers",
                operator: "equals",
            });
        }
        if let Some(services) = &clause.services {
            if !services.any.is_empty() {
                predicates.push(predicate(clause_set, clause_index, "services", "any"));
            }
            if !services.all.is_empty() {
                predicates.push(predicate(clause_set, clause_index, "services", "all"));
            }
        }
        if let Some(names) = &clause.names {
            if !names.exact.is_empty() {
                predicates.push(predicate(clause_set, clause_index, "names", "exact"));
            }
            if !names.prefixes.is_empty() {
                predicates.push(predicate(clause_set, clause_index, "names", "prefixes"));
            }
        }
        if let Some(data) = &clause.manufacturer_data {
            if !data.any.is_empty() {
                predicates.push(predicate(
                    clause_set,
                    clause_index,
                    "manufacturerData",
                    "any",
                ));
            }
            if !data.all.is_empty() {
                predicates.push(predicate(
                    clause_set,
                    clause_index,
                    "manufacturerData",
                    "all",
                ));
            }
        }
        if let Some(data) = &clause.service_data {
            if !data.any.is_empty() {
                predicates.push(predicate(clause_set, clause_index, "serviceData", "any"));
            }
            if !data.all.is_empty() {
                predicates.push(predicate(clause_set, clause_index, "serviceData", "all"));
            }
        }
        if let Some(rssi) = &clause.rssi {
            if rssi.minimum.is_some() {
                predicates.push(predicate(clause_set, clause_index, "rssi", "minimum"));
            }
            if rssi.maximum.is_some() {
                predicates.push(predicate(clause_set, clause_index, "rssi", "maximum"));
            }
        }
        if clause.connectable.is_some() {
            predicates.push(predicate(clause_set, clause_index, "connectable", "equals"));
        }
    }
    fn predicate(
        clause_set: &'static str,
        clause_index: usize,
        field: &'static str,
        operator: &'static str,
    ) -> Predicate {
        Predicate {
            clause_set,
            clause_index,
            field,
            operator,
        }
    }
}

fn observation_field_available(field: &str) -> bool {
    matches!(
        field,
        "localName" | "rssi" | "serviceUuids" | "manufacturerData" | "serviceData"
    )
}

fn query_wire(any_of: Option<&[Clause]>, exclude: Option<&[Clause]>, digest: &str) -> IpcValue {
    ipc_object([
        ("anyOf", clause_list_wire(any_of)),
        ("exclude", clause_list_wire(exclude)),
        ("digest", string(digest)),
    ])
}

fn clause_list_wire(clauses: Option<&[Clause]>) -> IpcValue {
    clauses.map_or(IpcValue::Null, |clauses| {
        IpcValue::Array(clauses.iter().map(clause_wire).collect())
    })
}

fn clause_wire(clause: &Clause) -> IpcValue {
    let mut entries = vec![
        (
            "peers",
            clause.peers.as_ref().map_or(IpcValue::Null, |peers| {
                IpcValue::Array(peers.iter().map(peer_wire).collect())
            }),
        ),
        (
            "services",
            clause
                .services
                .as_ref()
                .map_or(IpcValue::Null, uuid_field_wire),
        ),
        (
            "names",
            clause
                .names
                .as_ref()
                .map_or(IpcValue::Null, name_field_wire),
        ),
        (
            "manufacturerData",
            clause
                .manufacturer_data
                .as_ref()
                .map_or(IpcValue::Null, data_field_manufacturer_wire),
        ),
        (
            "serviceData",
            clause
                .service_data
                .as_ref()
                .map_or(IpcValue::Null, data_field_service_wire),
        ),
        (
            "rssi",
            clause.rssi.as_ref().map_or(IpcValue::Null, rssi_wire),
        ),
    ];
    entries.push((
        "connectable",
        clause.connectable.map_or(IpcValue::Null, IpcValue::Bool),
    ));
    ipc_object(entries)
}

fn peer_wire(peer: &PeerReference) -> IpcValue {
    ipc_object([
        ("version", IpcValue::Number(Number::from(1))),
        ("backendId", string(peer.backend_id.as_str())),
        ("scope", string(peer.scope.as_str())),
        ("opaqueId", string(peer.opaque_id.as_str())),
    ])
}

fn uuid_field_wire(field: &UuidField) -> IpcValue {
    ipc_object([
        (
            "any",
            IpcValue::Array(
                field
                    .any
                    .iter()
                    .map(|value| string(value.as_str()))
                    .collect(),
            ),
        ),
        (
            "all",
            IpcValue::Array(
                field
                    .all
                    .iter()
                    .map(|value| string(value.as_str()))
                    .collect(),
            ),
        ),
    ])
}

fn name_field_wire(field: &NameField) -> IpcValue {
    ipc_object([
        (
            "exact",
            IpcValue::Array(
                field
                    .exact
                    .iter()
                    .map(|value| string(value.as_str()))
                    .collect(),
            ),
        ),
        (
            "prefixes",
            IpcValue::Array(
                field
                    .prefixes
                    .iter()
                    .map(|value| string(value.as_str()))
                    .collect(),
            ),
        ),
    ])
}

fn data_field_manufacturer_wire(field: &DataField<ManufacturerPattern>) -> IpcValue {
    data_field_wire(&field.any, &field.all)
}

fn data_field_service_wire(field: &DataField<ServicePattern>) -> IpcValue {
    data_field_wire(&field.any, &field.all)
}

fn data_field_wire<Pattern: PatternWire>(any: &[Pattern], all: &[Pattern]) -> IpcValue {
    ipc_object([
        (
            "any",
            IpcValue::Array(any.iter().map(PatternWire::wire).collect()),
        ),
        (
            "all",
            IpcValue::Array(all.iter().map(PatternWire::wire).collect()),
        ),
    ])
}

trait PatternWire {
    fn wire(&self) -> IpcValue;
}

impl PatternWire for ManufacturerPattern {
    fn wire(&self) -> IpcValue {
        let mut entries = vec![("companyId", IpcValue::Number(Number::from(self.company_id)))];
        if let Some(prefix) = &self.data_prefix {
            entries.push(("dataPrefix", IpcValue::Bytes(prefix.clone())));
        }
        if let Some(mask) = &self.mask {
            entries.push(("mask", IpcValue::Bytes(mask.clone())));
        }
        ipc_object(entries)
    }
}

impl PatternWire for ServicePattern {
    fn wire(&self) -> IpcValue {
        let mut entries = vec![("service", string(self.service.as_str()))];
        if let Some(prefix) = &self.data_prefix {
            entries.push(("dataPrefix", IpcValue::Bytes(prefix.clone())));
        }
        if let Some(mask) = &self.mask {
            entries.push(("mask", IpcValue::Bytes(mask.clone())));
        }
        ipc_object(entries)
    }
}

fn rssi_wire(field: &RssiField) -> IpcValue {
    let mut entries = Vec::new();
    if let Some(minimum) = &field.minimum {
        entries.push(("minimum", IpcValue::Number(minimum.clone())));
    }
    if let Some(maximum) = &field.maximum {
        entries.push(("maximum", IpcValue::Number(maximum.clone())));
    }
    ipc_object(entries)
}

fn predicate_wire(predicate: &Predicate) -> IpcValue {
    ipc_object([
        ("clauseSet", string(predicate.clause_set)),
        (
            "clauseIndex",
            IpcValue::Number(Number::from(predicate.clause_index)),
        ),
        ("field", string(predicate.field)),
        ("operator", string(predicate.operator)),
    ])
}

fn canonical_query_json(any_of: Option<&[Clause]>, exclude: Option<&[Clause]>) -> String {
    format!(
        "{{\"anyOf\":{},\"exclude\":{}}}",
        canonical_clause_list_json(any_of),
        canonical_clause_list_json(exclude)
    )
}

fn canonical_clause_list_json(clauses: Option<&[Clause]>) -> String {
    clauses.map_or_else(
        || "null".to_owned(),
        |clauses| {
            format!(
                "[{}]",
                clauses
                    .iter()
                    .map(clause_canonical_json)
                    .collect::<Vec<_>>()
                    .join(",")
            )
        },
    )
}

fn clause_canonical_json(clause: &Clause) -> String {
    let mut output = String::from("{");
    let mut first = true;
    if let Some(peers) = &clause.peers {
        push_json_field(
            &mut output,
            &mut first,
            "peers",
            Some(format!(
                "[{}]",
                peers
                    .iter()
                    .map(peer_canonical_json)
                    .collect::<Vec<_>>()
                    .join(",")
            )),
        );
    }
    push_json_field(
        &mut output,
        &mut first,
        "services",
        clause.services.as_ref().map(uuid_field_canonical_json),
    );
    push_json_field(
        &mut output,
        &mut first,
        "names",
        clause.names.as_ref().map(name_field_canonical_json),
    );
    push_json_field(
        &mut output,
        &mut first,
        "manufacturerData",
        clause
            .manufacturer_data
            .as_ref()
            .map(data_field_canonical_json),
    );
    push_json_field(
        &mut output,
        &mut first,
        "serviceData",
        clause.service_data.as_ref().map(data_field_canonical_json),
    );
    push_json_field(
        &mut output,
        &mut first,
        "rssi",
        clause.rssi.as_ref().map(rssi_canonical_json),
    );
    if let Some(connectable) = clause.connectable {
        push_json_field(
            &mut output,
            &mut first,
            "connectable",
            Some(connectable.to_string()),
        );
    }
    output.push('}');
    output
}

fn peer_canonical_json(peer: &PeerReference) -> String {
    format!(
        "{{\"version\":1,\"backendId\":{},\"scope\":{},\"opaqueId\":{}}}",
        json_string(&peer.backend_id),
        json_string(&peer.scope),
        json_string(&peer.opaque_id)
    )
}

fn uuid_field_canonical_json(field: &UuidField) -> String {
    format!(
        "{{\"any\":[{}],\"all\":[{}]}}",
        field
            .any
            .iter()
            .map(|value| json_string(value))
            .collect::<Vec<_>>()
            .join(","),
        field
            .all
            .iter()
            .map(|value| json_string(value))
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn name_field_canonical_json(field: &NameField) -> String {
    format!(
        "{{\"exact\":[{}],\"prefixes\":[{}]}}",
        field
            .exact
            .iter()
            .map(|value| json_string(value))
            .collect::<Vec<_>>()
            .join(","),
        field
            .prefixes
            .iter()
            .map(|value| json_string(value))
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn data_field_canonical_json<Pattern: ParsePattern>(field: &DataField<Pattern>) -> String {
    format!(
        "{{\"any\":[{}],\"all\":[{}]}}",
        field
            .any
            .iter()
            .map(ParsePattern::canonical_json)
            .collect::<Vec<_>>()
            .join(","),
        field
            .all
            .iter()
            .map(ParsePattern::canonical_json)
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn rssi_canonical_json(field: &RssiField) -> String {
    let mut values = Vec::new();
    if let Some(minimum) = &field.minimum {
        values.push(format!("\"minimum\":{}", minimum));
    }
    if let Some(maximum) = &field.maximum {
        values.push(format!("\"maximum\":{}", maximum));
    }
    format!("{{{}}}", values.join(","))
}

fn push_json_field(output: &mut String, first: &mut bool, key: &str, value: Option<String>) {
    let Some(value) = value else {
        return;
    };
    if !*first {
        output.push(',');
    }
    *first = false;
    output.push_str(&json_string(key));
    output.push(':');
    output.push_str(&value);
}

fn predicate_canonical_json(predicate: &Predicate) -> String {
    format!(
        "{{\"clauseSet\":{},\"clauseIndex\":{},\"field\":{},\"operator\":{}}}",
        json_string(predicate.clause_set),
        predicate.clause_index,
        json_string(predicate.field),
        json_string(predicate.operator)
    )
}

fn scan_query_digest(canonical: &str) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for code_unit in canonical.encode_utf16() {
        hash ^= u64::from(code_unit);
        hash = hash.wrapping_mul(0x100000001b3_u64);
    }
    format!("scan-query-v1:{hash:016x}")
}

fn canonical_uuid(value: &str) -> Option<String> {
    let expanded = match value.len() {
        4 => format!("0000{value}-0000-1000-8000-00805f9b34fb"),
        8 => format!("{value}-0000-1000-8000-00805f9b34fb"),
        _ => value.to_owned(),
    };
    Uuid::parse_str(&expanded)
        .ok()
        .map(|value| value.to_string())
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("serializing a Rust string cannot fail")
}

fn exact_keys(
    object: &BTreeMap<String, IpcValue>,
    required: &[&str],
    optional: &[&str],
    operation: &str,
) -> Result<(), String> {
    if required.iter().any(|key| !object.contains_key(*key))
        || object
            .keys()
            .any(|key| !required.contains(&key.as_str()) && !optional.contains(&key.as_str()))
    {
        return Err(format!("{operation} contains an invalid key set"));
    }
    Ok(())
}

fn object_ref<'a>(
    value: &'a IpcValue,
    operation: &str,
) -> Result<&'a BTreeMap<String, IpcValue>, String> {
    match value {
        IpcValue::Object(value) => Ok(value),
        _ => Err(format!("{operation} must be an object")),
    }
}

fn string_field(
    object: &BTreeMap<String, IpcValue>,
    key: &str,
    operation: &str,
) -> Result<String, String> {
    match object.get(key) {
        Some(IpcValue::String(value)) if !value.is_empty() => Ok(value.clone()),
        _ => Err(format!("{operation} {key} is invalid")),
    }
}

fn number_field<'a>(
    object: &'a BTreeMap<String, IpcValue>,
    key: &str,
    operation: &str,
) -> Result<&'a Number, String> {
    match object.get(key) {
        Some(IpcValue::Number(value)) => Ok(value),
        _ => Err(format!("{operation} {key} is invalid")),
    }
}

fn string(value: &str) -> IpcValue {
    IpcValue::String(value.to_owned())
}

fn ipc_object<'a>(entries: impl IntoIterator<Item = (&'a str, IpcValue)>) -> IpcValue {
    IpcValue::Object(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query_with_digest(digest: &str) -> IpcValue {
        IpcValue::Object(
            [
                (
                    "anyOf".to_owned(),
                    IpcValue::Array(vec![ipc_object([
                        ("peers", IpcValue::Null),
                        (
                            "services",
                            ipc_object([
                                ("any", IpcValue::Array(Vec::new())),
                                (
                                    "all",
                                    IpcValue::Array(vec![string(
                                        "0000180d-0000-1000-8000-00805f9b34fb",
                                    )]),
                                ),
                            ]),
                        ),
                        ("names", IpcValue::Null),
                        ("manufacturerData", IpcValue::Null),
                        ("serviceData", IpcValue::Null),
                        ("rssi", IpcValue::Null),
                    ])]),
                ),
                ("exclude".to_owned(), IpcValue::Null),
                ("digest".to_owned(), string(digest)),
            ]
            .into_iter()
            .collect(),
        )
    }

    fn canonical_service_query_digest() -> String {
        let clause = Clause {
            peers: None,
            services: Some(UuidField {
                any: Vec::new(),
                all: vec!["0000180d-0000-1000-8000-00805f9b34fb".to_owned()],
            }),
            names: None,
            manufacturer_data: None,
            service_data: None,
            rssi: None,
            connectable: None,
        };
        scan_query_digest(&canonical_query_json(Some(&[clause]), None))
    }

    #[test]
    fn trusted_decoder_rejects_a_tampered_query_digest() {
        let error =
            decode_normalized_scan_query(&query_with_digest("scan-query-v1:0000000000000000"))
                .expect_err("tampered query digest must fail closed");
        assert_eq!(error, "normalized scan query digest is invalid");
    }

    #[test]
    fn trusted_plan_projects_only_common_positive_services_and_retains_residual() {
        let decoded =
            decode_normalized_scan_query(&query_with_digest(&canonical_service_query_digest()))
                .expect("canonical query must decode");
        let plan = diagnostic_scan_plan(&decoded);

        assert_eq!(
            decoded.native_service_uuids,
            vec!["0000180d-0000-1000-8000-00805f9b34fb"]
        );
        assert!(!matches!(plan, IpcValue::Object(ref value) if value.contains_key("nativeFilter")));
        let IpcValue::Object(fields) = plan else {
            panic!("diagnostic scan plan must be an object")
        };
        assert_eq!(
            fields.get("queryDigest"),
            Some(&string(decoded.digest.as_str()))
        );
        assert_eq!(
            fields.get("residualQueryDigest"),
            Some(&string(decoded.digest.as_str()))
        );
        assert!(fields.contains_key("residual"));
    }
}
