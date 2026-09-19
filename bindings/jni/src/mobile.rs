//! JNI facade of the process-owned mobile owner (`ubm-mobile`), class
//! `com.ubm.core.MobileCoreBridge` (see its Java doc for the frozen shape).
//!
//! No path from here reaches the staged/fake radio: this module uses only
//! `ubm_mobile` (guarded by `mobile_surface_has_no_staged_path`).
//!
//! Rust → Java: every [`RadioRequest`] becomes one typed `RadioHost` call
//! ([`request_call`] is the single mapping, unit-tested without a JVM).
//! A Java exception thrown by the host answers the request as a platform
//! failure instead of leaving it pending. Java → Rust: typed
//! `nativeComplete*` / `nativeIngest*` entries rebuild the typed values.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use jni::errors::{Error as JniError, ErrorPolicy};
use jni::objects::{
    JBooleanArray, JByteArray, JClass, JIntArray, JLongArray, JObject, JObjectArray, JString,
    Reference as _,
};
use jni::refs::Global;
use jni::signature::{MethodSignature, RuntimeMethodSignature};
use jni::strings::JNIString;
use jni::sys::{jboolean, jint, jlong, jstring};
use jni::{Env, EnvUnowned, JValue, JavaVM};
use ubm_desktop::{
    CharacteristicSnapshot, DeliveryMode, DescriptorSnapshot, DesktopError, ObservedDelivery,
    PropertyFlags, ReadProvenance, ServiceSnapshot,
};
use ubm_mobile::{
    AdapterAuthorization, AdapterAvailability, AdapterPower, AdapterSnapshot, Advertisement,
    AuthenticationState, BondState, BondedPeer, CloseFailure, CompletionStatus, EncryptionState,
    FailureKind, HostOptions, IngressClass, IngressStatus, Instance, ManufacturerData, MobileHost,
    MobilePlatform, PhyObservation, PlatformFailure, PlatformRadio, RadioCompletion, RadioIngress,
    RadioRequest, RestoredPeer, SecureConnectionsState, SecurityState, ServiceData, WakeSink,
};

use crate::build_identity::ubm_build_identity_json;

const EXCEPTION_CLASS: &str = "com/ubm/core/MobileCoreBridge$MobileCoreException";
/// `MobileCoreBridge.ABSENT_INT`.
const ABSENT_INT: jint = jint::MIN;
/// `MobileCoreBridge.STATUS_NO_HOST`.
const STATUS_NO_HOST: jint = -1;

// -- process host slot -------------------------------------------------

fn host_slot() -> &'static Mutex<Option<MobileHost>> {
    static SLOT: OnceLock<Mutex<Option<MobileHost>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

fn current_host() -> Option<MobileHost> {
    host_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

// -- errors ------------------------------------------------------------

#[derive(Debug)]
enum MobileError {
    Contract(DesktopError),
    Jni(JniError),
}

impl From<JniError> for MobileError {
    fn from(error: JniError) -> Self {
        Self::Jni(error)
    }
}

impl From<DesktopError> for MobileError {
    fn from(error: DesktopError) -> Self {
        Self::Contract(error)
    }
}

type MobileResult<T> = Result<T, MobileError>;

fn wire_message(error: &DesktopError) -> String {
    format!(
        "{}|{}|{}|{}",
        error.code_str(),
        error.domain().as_str(),
        error.operation(),
        error.detail().unwrap_or("")
    )
}

fn invalid(operation: &str, detail: &str) -> MobileError {
    MobileError::Contract(
        DesktopError::new(
            ubm_core::contracts::BleErrorCode::ArgumentInvalid,
            ubm_core::contracts::BleErrorDomain::Core,
            operation,
        )
        .with_detail(detail.to_owned()),
    )
}

fn throw(env: &mut Env, message: &str) {
    let result = env.throw_new(JNIString::from(EXCEPTION_CLASS), JNIString::from(message));
    if result.is_err() && !env.exception_check() {
        let _ = env.throw_new(
            JNIString::from("java/lang/RuntimeException"),
            JNIString::from(message),
        );
    }
}

/// Every `Err` and panic becomes a typed `MobileCoreException`; the native
/// returns its default.
struct ThrowMobile;

impl<T: Default> ErrorPolicy<T, MobileError> for ThrowMobile {
    type Captures<'unowned_env_local: 'native_method, 'native_method> = &'static str;

    fn on_error<'unowned_env_local: 'native_method, 'native_method>(
        env: &mut Env<'unowned_env_local>,
        operation: &mut Self::Captures<'unowned_env_local, 'native_method>,
        err: MobileError,
    ) -> jni::errors::Result<T> {
        match err {
            MobileError::Contract(error) => throw(env, &wire_message(&error)),
            MobileError::Jni(error) => {
                eprintln!("ubm-mobile jni [{operation}]: {error:?}");
                throw(
                    env,
                    &format!("platform.failure|platform|{operation}|jni-call-failed"),
                );
            }
        }
        Ok(T::default())
    }

    fn on_panic<'unowned_env_local: 'native_method, 'native_method>(
        env: &mut Env<'unowned_env_local>,
        operation: &mut Self::Captures<'unowned_env_local, 'native_method>,
        _payload: Box<dyn std::any::Any + Send + 'static>,
    ) -> jni::errors::Result<T> {
        throw(
            env,
            &format!("lifecycle.invariant-violation|core|{operation}|rust-panic-contained"),
        );
        Ok(T::default())
    }
}

// -- Rust → Java request mapping ----------------------------------------

/// One typed argument of a `RadioHost` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arg {
    Long(i64),
    Int(i32),
    Bool(bool),
    Str(Option<String>),
    Bytes(Vec<u8>),
    Strings(Vec<String>),
}

impl Arg {
    const fn descriptor(&self) -> &'static str {
        match self {
            Self::Long(_) => "J",
            Self::Int(_) => "I",
            Self::Bool(_) => "Z",
            Self::Str(_) => "Ljava/lang/String;",
            Self::Bytes(_) => "[B",
            Self::Strings(_) => "[Ljava/lang/String;",
        }
    }
}

/// The JNI signature of a `void` method taking `args`.
#[must_use]
pub fn signature(args: &[Arg]) -> String {
    let mut out = String::from("(");
    for arg in args {
        out.push_str(arg.descriptor());
    }
    out.push_str(")V");
    out
}

fn id(value: u64) -> Arg {
    Arg::Long(i64::try_from(value).unwrap_or(i64::MAX))
}

fn text(value: &str) -> Arg {
    Arg::Str(Some(value.to_owned()))
}

fn occurrence(value: u64) -> Arg {
    id(value)
}

fn instance_args(instance: &Instance) -> Vec<Arg> {
    vec![
        text(&instance.peer_id),
        text(&instance.service_uuid),
        occurrence(instance.service_occurrence),
        text(&instance.characteristic_uuid),
        occurrence(instance.characteristic_occurrence),
    ]
}

fn delivery(mode: Option<DeliveryMode>) -> Arg {
    Arg::Str(mode.map(|mode| mode.as_str().to_owned()))
}

/// The `RadioHost` method and arguments for one request.
#[must_use]
pub fn request_call(request: &RadioRequest) -> (&'static str, Vec<Arg>) {
    let rid = id(request.id());
    match request {
        RadioRequest::AdapterState { .. } => ("adapterState", vec![rid]),
        RadioRequest::StartScan { scan, .. } => {
            let android = scan.android.unwrap_or_default();
            (
                "startScan",
                vec![
                    rid,
                    Arg::Strings(scan.service_uuids.clone()),
                    Arg::Strings(scan.device_addresses.clone()),
                    Arg::Str(android.mode.map(|mode| mode.as_str().to_owned())),
                    Arg::Str(android.callback_type.map(|kind| kind.as_str().to_owned())),
                    Arg::Int(android.legacy.map_or(-1, i32::from)),
                ],
            )
        }
        RadioRequest::StopScan { .. } => ("stopScan", vec![rid]),
        RadioRequest::Connect {
            peer_id,
            auto_connect,
            preferred_phy,
            ..
        } => (
            "connect",
            vec![
                rid,
                text(peer_id),
                Arg::Bool(*auto_connect),
                Arg::Strings(
                    preferred_phy
                        .iter()
                        .map(|phy| phy.as_str().to_owned())
                        .collect(),
                ),
            ],
        ),
        RadioRequest::Disconnect { peer_id, .. } => ("disconnect", vec![rid, text(peer_id)]),
        RadioRequest::Discover { peer_id, .. } => ("discover", vec![rid, text(peer_id)]),
        RadioRequest::Read { instance, .. } => {
            let mut args = vec![rid];
            args.extend(instance_args(instance));
            ("read", args)
        }
        RadioRequest::Write {
            instance,
            value,
            with_response,
            ..
        } => {
            let mut args = vec![rid];
            args.extend(instance_args(instance));
            args.push(Arg::Bytes(value.clone()));
            args.push(Arg::Bool(*with_response));
            ("write", args)
        }
        RadioRequest::ReadDescriptor { descriptor, .. } => {
            let mut args = vec![rid];
            args.extend(instance_args(&descriptor.instance));
            args.push(text(&descriptor.descriptor_uuid));
            args.push(occurrence(descriptor.descriptor_occurrence));
            ("readDescriptor", args)
        }
        RadioRequest::WriteDescriptor {
            descriptor, value, ..
        } => {
            let mut args = vec![rid];
            args.extend(instance_args(&descriptor.instance));
            args.push(text(&descriptor.descriptor_uuid));
            args.push(occurrence(descriptor.descriptor_occurrence));
            args.push(Arg::Bytes(value.clone()));
            ("writeDescriptor", args)
        }
        RadioRequest::EnableNotifications {
            instance,
            epoch,
            requested,
            preferred,
            ..
        } => {
            let mut args = vec![rid];
            args.extend(instance_args(instance));
            args.push(id(*epoch));
            args.push(delivery(*requested));
            args.push(delivery(*preferred));
            ("enableNotifications", args)
        }
        RadioRequest::DisableNotifications { instance, .. } => {
            let mut args = vec![rid];
            args.extend(instance_args(instance));
            ("disableNotifications", args)
        }
        RadioRequest::ReadMtu { peer_id, .. } => ("readMtu", vec![rid, text(peer_id)]),
        RadioRequest::ReadWriteLimits { peer_id, .. } => {
            ("readWriteLimits", vec![rid, text(peer_id)])
        }
        RadioRequest::RequestMtu { peer_id, mtu, .. } => (
            "requestMtu",
            vec![rid, text(peer_id), Arg::Int(i32::from(*mtu))],
        ),
        RadioRequest::ReadRssi { peer_id, .. } => ("readRssi", vec![rid, text(peer_id)]),
        RadioRequest::RequestConnectionPriority {
            peer_id, priority, ..
        } => (
            "requestConnectionPriority",
            vec![rid, text(peer_id), text(priority.as_str())],
        ),
        RadioRequest::ReadPhy { peer_id, .. } => ("readPhy", vec![rid, text(peer_id)]),
        RadioRequest::RequestPhy {
            peer_id, tx, rx, ..
        } => (
            "requestPhy",
            vec![
                rid,
                text(peer_id),
                Arg::Str(tx.map(|phy| phy.as_str().to_owned())),
                Arg::Str(rx.map(|phy| phy.as_str().to_owned())),
            ],
        ),
        RadioRequest::SecurityState { peer_id, .. } => ("securityState", vec![rid, text(peer_id)]),
        RadioRequest::CreateBond {
            peer_id, transport, ..
        } => (
            "createBond",
            vec![rid, text(peer_id), text(transport.as_str())],
        ),
        RadioRequest::CancelBond { peer_id, .. } => ("cancelBond", vec![rid, text(peer_id)]),
        RadioRequest::BondedPeers { .. } => ("bondedPeers", vec![rid]),
        RadioRequest::AcquireBackground { kind, reason, .. } => (
            "acquireBackground",
            vec![rid, text(kind.as_str()), text(reason)],
        ),
        RadioRequest::ReleaseBackground { lease_id, .. } => {
            ("releaseBackground", vec![rid, text(lease_id)])
        }
        RadioRequest::UpdateBackgroundNotification {
            lease_id,
            title,
            body,
            ..
        } => (
            "updateBackgroundNotification",
            vec![rid, text(lease_id), text(title), Arg::Str(body.clone())],
        ),
        RadioRequest::AssociateCompanion {
            name, service_uuid, ..
        } => (
            "associateCompanion",
            vec![rid, Arg::Str(name.clone()), Arg::Str(service_uuid.clone())],
        ),
        RadioRequest::ObservePresence { peer_id, .. } => {
            ("observePresence", vec![rid, text(peer_id)])
        }
        RadioRequest::StopPresence { peer_id, .. } => {
            ("unobservePresence", vec![rid, text(peer_id)])
        }
        RadioRequest::Close { .. } => ("close", vec![rid]),
    }
}

fn call_host(
    env: &mut Env,
    target: &JObject,
    method: &str,
    args: &[Arg],
) -> jni::errors::Result<()> {
    let mut objects: Vec<JObject> = Vec::with_capacity(args.len());
    for arg in args {
        let object = match arg {
            Arg::Str(Some(value)) => JObject::from(env.new_string(value)?),
            Arg::Bytes(value) => JObject::from(env.byte_array_from_slice(value)?),
            Arg::Strings(values) => {
                let array = JObjectArray::<JString>::new(env, values.len(), JString::null())?;
                for (index, value) in values.iter().enumerate() {
                    let element = env.new_string(value)?;
                    array.set_element(env, index, &element)?;
                }
                JObject::from(array)
            }
            Arg::Str(None) | Arg::Long(_) | Arg::Int(_) | Arg::Bool(_) => JObject::null(),
        };
        objects.push(object);
    }
    let values: Vec<JValue> = args
        .iter()
        .zip(objects.iter())
        .map(|(arg, object)| match arg {
            Arg::Long(value) => JValue::Long(*value),
            Arg::Int(value) => JValue::Int(*value),
            Arg::Bool(value) => JValue::Bool(*value),
            Arg::Str(_) | Arg::Bytes(_) | Arg::Strings(_) => JValue::Object(object),
        })
        .collect();
    let signature = RuntimeMethodSignature::from_str(signature(args))?;
    env.call_method(
        target,
        JNIString::from(method),
        MethodSignature::from(&signature),
        &values,
    )?;
    Ok(())
}

/// Counts host callbacks that failed (Java threw, or the JVM refused):
/// each also answers its request as a platform failure.
static CALLBACK_FAILURES: AtomicU64 = AtomicU64::new(0);

struct JniRadio {
    vm: JavaVM,
    host: Global<JObject<'static>>,
    owner: OnceLock<MobileHost>,
}

impl JniRadio {
    fn invoke(&self, method: &str, args: &[Arg]) -> Result<(), JniError> {
        self.vm
            .attach_current_thread(|env| call_host(env, self.host.as_obj(), method, args))
    }
}

impl PlatformRadio for JniRadio {
    fn submit(&self, request: RadioRequest) {
        let (method, args) = request_call(&request);
        if let Err(error) = self.invoke(method, &args) {
            CALLBACK_FAILURES.fetch_add(1, Ordering::Relaxed);
            let failure = PlatformFailure::new(
                FailureKind::Platform,
                format!("RadioHost.{method} failed: {error:?}"),
            );
            match self.owner.get() {
                Some(host) => {
                    host.complete(request.id(), RadioCompletion::Failed(failure));
                }
                None => eprintln!("ubm-mobile jni: {method} failed before the host existed"),
            }
        }
    }

    fn cancel(&self, request_id: u64) {
        if let Err(error) = self.invoke("cancel", &[id(request_id)]) {
            CALLBACK_FAILURES.fetch_add(1, Ordering::Relaxed);
            eprintln!("ubm-mobile jni: RadioHost.cancel({request_id}) failed: {error:?}");
        }
    }
}

struct JniWake {
    vm: JavaVM,
    listener: Global<JObject<'static>>,
}

impl WakeSink for JniWake {
    fn wake(&self, session_id: u64) {
        let result = self.vm.attach_current_thread(|env| {
            call_host(env, self.listener.as_obj(), "onWake", &[id(session_id)])
        });
        if let Err(error) = result {
            CALLBACK_FAILURES.fetch_add(1, Ordering::Relaxed);
            eprintln!("ubm-mobile jni: WakeListener.onWake({session_id}) failed: {error:?}");
        }
    }
}

// -- Java → Rust readers -------------------------------------------------

fn read_text(env: &mut Env, value: &JString, operation: &'static str) -> MobileResult<String> {
    read_opt_text(env, value, operation)?.ok_or_else(|| invalid(operation, "null string"))
}

fn read_opt_text(
    env: &mut Env,
    value: &JString,
    operation: &'static str,
) -> MobileResult<Option<String>> {
    if value.as_raw().is_null() {
        return Ok(None);
    }
    value
        .try_to_string(env)
        .map(Some)
        .map_err(|_| invalid(operation, "string is not valid UTF-8"))
}

fn read_texts(
    env: &mut Env,
    array: &JObjectArray<JString>,
    operation: &'static str,
) -> MobileResult<Vec<Option<String>>> {
    if array.as_raw().is_null() {
        return Ok(Vec::new());
    }
    let length = array.len(env)?;
    let mut out = Vec::with_capacity(length);
    for index in 0..length {
        let element = array.get_element(env, index)?;
        out.push(read_opt_text(env, &element, operation)?);
    }
    Ok(out)
}

fn read_required_texts(
    env: &mut Env,
    array: &JObjectArray<JString>,
    operation: &'static str,
) -> MobileResult<Vec<String>> {
    read_texts(env, array, operation)?
        .into_iter()
        .map(|entry| entry.ok_or_else(|| invalid(operation, "null array entry")))
        .collect()
}

/// A null array is "not reported"; an empty one is "reported, none".
fn read_opt_texts(
    env: &mut Env,
    array: &JObjectArray<JString>,
    operation: &'static str,
) -> MobileResult<Option<Vec<String>>> {
    if array.as_raw().is_null() {
        return Ok(None);
    }
    read_required_texts(env, array, operation).map(Some)
}

fn read_bytes(env: &mut Env, array: &JByteArray, operation: &'static str) -> MobileResult<Vec<u8>> {
    if array.as_raw().is_null() {
        return Err(invalid(operation, "null byte array"));
    }
    Ok(env.convert_byte_array(array)?)
}

fn read_byte_arrays(
    env: &mut Env,
    array: &JObjectArray<JByteArray>,
    operation: &'static str,
) -> MobileResult<Vec<Vec<u8>>> {
    if array.as_raw().is_null() {
        return Ok(Vec::new());
    }
    let length = array.len(env)?;
    let mut out = Vec::with_capacity(length);
    for index in 0..length {
        let element = array.get_element(env, index)?;
        out.push(read_bytes(env, &element, operation)?);
    }
    Ok(out)
}

fn read_ints(env: &mut Env, array: &JIntArray) -> MobileResult<Vec<i32>> {
    if array.as_raw().is_null() {
        return Ok(Vec::new());
    }
    let mut out = vec![0; array.len(env)?];
    array.get_region(env, 0, &mut out)?;
    Ok(out)
}

fn read_longs(env: &mut Env, array: &JLongArray) -> MobileResult<Vec<i64>> {
    if array.as_raw().is_null() {
        return Ok(Vec::new());
    }
    let mut out = vec![0; array.len(env)?];
    array.get_region(env, 0, &mut out)?;
    Ok(out)
}

fn read_bools(env: &mut Env, array: &JBooleanArray) -> MobileResult<Vec<bool>> {
    if array.as_raw().is_null() {
        return Ok(Vec::new());
    }
    let mut out = vec![false; array.len(env)?];
    array.get_region(env, 0, &mut out)?;
    Ok(out)
}

fn same_length(operation: &'static str, lengths: &[usize]) -> MobileResult<()> {
    if lengths.windows(2).all(|pair| pair[0] == pair[1]) {
        Ok(())
    } else {
        Err(invalid(operation, "parallel arrays differ in length"))
    }
}

fn to_u64(value: i64, operation: &'static str) -> MobileResult<u64> {
    u64::try_from(value).map_err(|_| invalid(operation, "negative occurrence or id"))
}

fn optional_int(value: jint) -> Option<i32> {
    (value != ABSENT_INT).then_some(value)
}

fn optional_i16(value: jint, operation: &'static str) -> MobileResult<Option<i16>> {
    optional_int(value)
        .map(|value| i16::try_from(value).map_err(|_| invalid(operation, "value out of range")))
        .transpose()
}

fn parse_enum<T>(
    value: &str,
    parse: fn(&str) -> Option<T>,
    operation: &'static str,
) -> MobileResult<T> {
    parse(value).ok_or_else(|| invalid(operation, "unknown enum value"))
}

fn adapter_snapshot(
    env: &mut Env,
    availability: &JString,
    authorization: &JString,
    power: &JString,
    safe_reason: &JString,
    operation: &'static str,
) -> MobileResult<AdapterSnapshot> {
    let availability = read_text(env, availability, operation)?;
    let authorization = read_text(env, authorization, operation)?;
    let power = read_text(env, power, operation)?;
    Ok(AdapterSnapshot {
        availability: parse_enum(&availability, AdapterAvailability::parse, operation)?,
        authorization: parse_enum(&authorization, AdapterAuthorization::parse, operation)?,
        power: parse_enum(&power, AdapterPower::parse, operation)?,
        safe_reason: read_opt_text(env, safe_reason, operation)?,
    })
}

#[allow(clippy::too_many_arguments)]
fn security_state(
    env: &mut Env,
    bond: &JString,
    encryption: &JString,
    authentication: &JString,
    secure_connections: &JString,
    pairing_possible: jint,
    operation: &'static str,
) -> MobileResult<SecurityState> {
    let bond = read_text(env, bond, operation)?;
    let encryption = read_text(env, encryption, operation)?;
    let authentication = read_text(env, authentication, operation)?;
    let secure_connections = read_text(env, secure_connections, operation)?;
    Ok(SecurityState {
        bond: parse_enum(&bond, BondState::parse, operation)?,
        encryption: parse_enum(&encryption, EncryptionState::parse, operation)?,
        authentication: parse_enum(&authentication, AuthenticationState::parse, operation)?,
        secure_connections: parse_enum(
            &secure_connections,
            SecureConnectionsState::parse,
            operation,
        )?,
        pairing_possible: match pairing_possible {
            -1 => None,
            0 => Some(false),
            1 => Some(true),
            _ => return Err(invalid(operation, "pairingPossible must be -1, 0 or 1")),
        },
    })
}

/// Android `BluetoothGattCharacteristic.PROPERTY_*` bits → flags.
#[must_use]
pub fn android_properties(bits: i32) -> PropertyFlags {
    PropertyFlags {
        read: bits & 0x02 != 0,
        write_without_response: bits & 0x04 != 0,
        write: bits & 0x08 != 0,
        notify: bits & 0x10 != 0,
        indicate: bits & 0x20 != 0,
    }
}

/// Rebuild the discovery tree from its pre-order encoding.
pub fn discovery_tree(
    levels: &[i32],
    uuids: &[String],
    occurrences: &[i64],
    properties: &[i32],
) -> Result<Vec<ServiceSnapshot>, &'static str> {
    if levels.len() != uuids.len()
        || levels.len() != occurrences.len()
        || levels.len() != properties.len()
    {
        return Err("parallel arrays differ in length");
    }
    let mut services: Vec<ServiceSnapshot> = Vec::new();
    for index in 0..levels.len() {
        let occurrence = u64::try_from(occurrences[index]).map_err(|_| "negative occurrence")?;
        let uuid = uuids[index].clone();
        match levels[index] {
            0 => services.push(ServiceSnapshot {
                uuid,
                occurrence,
                characteristics: Vec::new(),
            }),
            1 => services
                .last_mut()
                .ok_or("characteristic before any service")?
                .characteristics
                .push(CharacteristicSnapshot {
                    uuid,
                    occurrence,
                    properties: android_properties(properties[index]),
                    descriptors: Vec::new(),
                }),
            2 => services
                .last_mut()
                .and_then(|service| service.characteristics.last_mut())
                .ok_or("descriptor before any characteristic")?
                .descriptors
                .push(DescriptorSnapshot { uuid, occurrence }),
            _ => return Err("level must be 0, 1 or 2"),
        }
    }
    Ok(services)
}

fn completion_status(status: CompletionStatus) -> jint {
    match status {
        CompletionStatus::Delivered => 0,
        CompletionStatus::Late => 1,
        CompletionStatus::Mismatched => 2,
    }
}

fn ingress_status(status: IngressStatus) -> jint {
    match status {
        IngressStatus::Accepted => 0,
        IngressStatus::Dropped(IngressClass::Advertisement) => 1,
        IngressStatus::Dropped(IngressClass::Notification) => 2,
        IngressStatus::Dropped(IngressClass::Control) => 3,
        IngressStatus::Closed => 4,
    }
}

fn complete(request_id: jlong, completion: RadioCompletion) -> jint {
    match (current_host(), u64::try_from(request_id)) {
        (Some(host), Ok(request_id)) => completion_status(host.complete(request_id, completion)),
        (Some(_), Err(_)) => completion_status(CompletionStatus::Late),
        (None, _) => STATUS_NO_HOST,
    }
}

fn ingest(ingress: RadioIngress) -> jint {
    match current_host() {
        Some(host) => ingress_status(host.ingest(ingress)),
        None => STATUS_NO_HOST,
    }
}

/// A Java value that cannot be read answers the request as a platform
/// failure (the request never stays pending on a malformed answer).
fn complete_or_fail(request_id: jlong, built: MobileResult<RadioCompletion>) -> jint {
    let completion = built.unwrap_or_else(|error| {
        RadioCompletion::Failed(PlatformFailure::new(
            FailureKind::Platform,
            format!("malformed completion from RadioHost: {error:?}"),
        ))
    });
    complete(request_id, completion)
}

fn instance(
    env: &mut Env,
    peer_id: &JString,
    service_uuid: &JString,
    service_occurrence: jlong,
    characteristic_uuid: &JString,
    characteristic_occurrence: jlong,
    operation: &'static str,
) -> MobileResult<Instance> {
    Ok(Instance {
        peer_id: read_text(env, peer_id, operation)?,
        service_uuid: read_text(env, service_uuid, operation)?,
        service_occurrence: to_u64(service_occurrence, operation)?,
        characteristic_uuid: read_text(env, characteristic_uuid, operation)?,
        characteristic_occurrence: to_u64(characteristic_occurrence, operation)?,
    })
}

fn contract_revision() -> &'static str {
    ubm_core::contracts::CONTRACT_REVISION
}

// -- natives: identity ------------------------------------------------

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeBuildIdentityJson<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> jstring {
    unowned_env
        .with_env(|env| -> MobileResult<jstring> {
            Ok(env
                .new_string(ubm_build_identity_json(contract_revision()))?
                .into_raw())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.build-identity")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeContractRevision<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> jstring {
    unowned_env
        .with_env(|env| -> MobileResult<jstring> {
            Ok(env.new_string(contract_revision())?.into_raw())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.contract-revision")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeWireRevision<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> jstring {
    unowned_env
        .with_env(|env| -> MobileResult<jstring> {
            Ok(env.new_string(ubm_mobile::WIRE_REVISION)?.into_raw())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.wire-revision")
}

// -- natives: host ------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeInstallHost<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    radio: JObject<'caller>,
    wake: JObject<'caller>,
    platform: JString<'caller>,
    owner: JString<'caller>,
    adapter_label: JString<'caller>,
) {
    unowned_env
        .with_env(|env| -> MobileResult<()> {
            const OP: &str = "mobile.host.install";
            if radio.as_raw().is_null() || wake.as_raw().is_null() {
                return Err(invalid(OP, "radio and wake listener are required"));
            }
            let platform = match read_text(env, &platform, OP)?.as_str() {
                "android" => MobilePlatform::Android,
                "apple" => MobilePlatform::Apple,
                _ => return Err(invalid(OP, "platform must be android or apple")),
            };
            let owner = read_text(env, &owner, OP)?;
            let adapter_label = read_text(env, &adapter_label, OP)?;
            let mut slot = host_slot()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if slot.is_some() {
                return Err(MobileError::Contract(
                    DesktopError::new(
                        ubm_core::contracts::BleErrorCode::LifecycleInvalidState,
                        ubm_core::contracts::BleErrorDomain::Core,
                        OP,
                    )
                    .with_detail("a mobile host is already installed in this process"),
                ));
            }
            let jni_radio = Arc::new(JniRadio {
                vm: env.get_java_vm()?,
                host: env.new_global_ref(&radio)?,
                owner: OnceLock::new(),
            });
            let jni_wake = Arc::new(JniWake {
                vm: env.get_java_vm()?,
                listener: env.new_global_ref(&wake)?,
            });
            let host = MobileHost::open_blocking(
                Arc::clone(&jni_radio) as Arc<dyn PlatformRadio>,
                jni_wake,
                HostOptions {
                    platform,
                    owner,
                    adapter_label,
                },
                ubm_desktop::executor::desktop_runtime(),
            )?;
            let _ = jni_radio.owner.set(host.clone());
            *slot = Some(host);
            Ok(())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.host.install")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeHostInstalled<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> jboolean {
    unowned_env
        .with_env(|_env| -> MobileResult<jboolean> { Ok(current_host().is_some()) })
        .resolve_with::<ThrowMobile, _>(|| "mobile.host.installed")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeShutdownHost<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> jstring {
    unowned_env
        .with_env(|env| -> MobileResult<jstring> {
            // The host stays installed while it shuts down: the platform's
            // answers to the teardown requests (Close) route through it.
            let record = match current_host() {
                Some(host) => {
                    let record = host.shutdown_blocking();
                    let mut slot = host_slot()
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    *slot = None;
                    record
                }
                None => String::from("{\"failures\":[],\"state\":\"released\"}"),
            };
            Ok(env.new_string(record)?.into_raw())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.host.shutdown")
}

// -- natives: sessions --------------------------------------------------

fn require_host(operation: &'static str) -> MobileResult<MobileHost> {
    current_host().ok_or_else(|| {
        MobileError::Contract(
            DesktopError::new(
                ubm_core::contracts::BleErrorCode::LifecycleDestroyed,
                ubm_core::contracts::BleErrorDomain::Core,
                operation,
            )
            .with_detail("no mobile host is installed"),
        )
    })
}

fn require_session(
    session_id: jlong,
    operation: &'static str,
) -> MobileResult<ubm_mobile::MobileSession> {
    let host = require_host(operation)?;
    u64::try_from(session_id)
        .ok()
        .and_then(|id| host.session(id))
        .ok_or_else(|| {
            MobileError::Contract(
                DesktopError::new(
                    ubm_core::contracts::BleErrorCode::LifecycleDestroyed,
                    ubm_core::contracts::BleErrorDomain::Core,
                    operation,
                )
                .with_detail("unknown or disposed session"),
            )
        })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeOpenSession<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    owner: JString<'caller>,
    expected_wire_revision: JString<'caller>,
    background_scope: JString<'caller>,
) -> jstring {
    unowned_env
        .with_env(|env| -> MobileResult<jstring> {
            const OP: &str = "mobile.session.open";
            let owner = read_text(env, &owner, OP)?;
            let expected = read_text(env, &expected_wire_revision, OP)?;
            let scope = read_text(env, &background_scope, OP)?;
            let host = require_host(OP)?;
            let (_, record) = host.admit_scoped_session(
                &owner,
                &scope,
                &expected,
                &ubm_build_identity_json(contract_revision()),
            )?;
            Ok(env.new_string(record)?.into_raw())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.session.open")
}

/// Ends one background scope (React Native module invalidation): releases
/// every foreground-service lease its sessions acquired. Returns the
/// cleanup record JSON; with no host installed nothing is held.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeReleaseBackgroundScope<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    background_scope: JString<'caller>,
) -> jstring {
    unowned_env
        .with_env(|env| -> MobileResult<jstring> {
            const OP: &str = "mobile.background.release-scope";
            let scope = read_text(env, &background_scope, OP)?;
            let record = match current_host() {
                Some(host) => host.release_background_scope_blocking(&scope),
                None => String::from("{\"failures\":[],\"state\":\"released\"}"),
            };
            Ok(env.new_string(record)?.into_raw())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.background.release-scope")
}

struct JniCallback {
    vm: JavaVM,
    callback: Global<JObject<'static>>,
}

impl JniCallback {
    fn deliver(self, envelope: String) {
        let result = self
            .vm
            .attach_current_thread(|env| -> jni::errors::Result<()> {
                let text = env.new_string(&envelope)?;
                let signature = RuntimeMethodSignature::from_str("(Ljava/lang/String;)V")?;
                env.call_method(
                    self.callback.as_obj(),
                    JNIString::from("onResult"),
                    MethodSignature::from(&signature),
                    &[JValue::Object(&text)],
                )?;
                Ok(())
            });
        if let Err(error) = result {
            CALLBACK_FAILURES.fetch_add(1, Ordering::Relaxed);
            eprintln!("ubm-mobile jni: InvokeCallback.onResult failed: {error:?}");
        }
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeInvoke<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    session_id: jlong,
    op: JString<'caller>,
    args_json: JString<'caller>,
    callback: JObject<'caller>,
) {
    unowned_env
        .with_env(|env| -> MobileResult<()> {
            const OP: &str = "mobile.session.invoke";
            if callback.as_raw().is_null() {
                return Err(invalid(OP, "callback is required"));
            }
            let session = require_session(session_id, OP)?;
            let op = read_text(env, &op, OP)?;
            let args = read_text(env, &args_json, OP)?;
            let callback = JniCallback {
                vm: env.get_java_vm()?,
                callback: env.new_global_ref(&callback)?,
            };
            session.invoke(
                &op,
                &args,
                Box::new(move |envelope| callback.deliver(envelope)),
            );
            Ok(())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.session.invoke")
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_MobileCoreBridge_nativeDrain<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    session_id: jlong,
    max_items: jint,
    max_bytes: jint,
) -> jstring {
    unowned_env
        .with_env(|env| -> MobileResult<jstring> {
            const OP: &str = "mobile.session.drain";
            let session = require_session(session_id, OP)?;
            let max_items = u32::try_from(max_items).map_err(|_| invalid(OP, "maxItems"))?;
            let max_bytes = u32::try_from(max_bytes).map_err(|_| invalid(OP, "maxBytes"))?;
            Ok(env
                .new_string(session.drain(max_items, max_bytes))?
                .into_raw())
        })
        .resolve_with::<ThrowMobile, _>(|| "mobile.session.drain")
}

// -- natives: completions -----------------------------------------------

macro_rules! completion_native {
    ($name:ident, $op:literal, ($($param:ident : $ty:ty),*), |$env:ident| $body:expr) => {
        // The immediately-called closure scopes `?` to the value being
        // built, so a malformed Java value fails the request, not the call.
        #[allow(clippy::redundant_closure_call)]
        #[unsafe(no_mangle)]
        pub extern "system" fn $name<'caller>(
            mut unowned_env: EnvUnowned<'caller>,
            _class: JClass<'caller>,
            request_id: jlong,
            $($param: $ty),*
        ) -> jint {
            unowned_env
                .with_env(|$env| -> MobileResult<jint> {
                    let built: MobileResult<RadioCompletion> = (|| $body)();
                    Ok(complete_or_fail(request_id, built))
                })
                .resolve_with::<ThrowMobile, _>(|| $op)
        }
    };
}

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteUnit,
    "mobile.complete.unit",
    (),
    |_env| Ok(RadioCompletion::Unit)
);

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteBytes,
    "mobile.complete.bytes",
    (value: JByteArray<'caller>),
    |env| Ok(RadioCompletion::Bytes(read_bytes(env, &value, "mobile.complete.bytes")?))
);

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteRead,
    "mobile.complete.read",
    (value: JByteArray<'caller>, provenance: JString<'caller>),
    |env| {
        const OP: &str = "mobile.complete.read";
        let provenance = ReadProvenance::from_wire(read_text(env, &provenance, OP)?.as_str())
            .ok_or_else(|| invalid(OP, "provenance must be read-response or read-or-notification"))?;
        Ok(RadioCompletion::Read {
            value: read_bytes(env, &value, OP)?,
            provenance,
        })
    }
);

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteAdapter,
    "mobile.complete.adapter",
    (availability: JString<'caller>, authorization: JString<'caller>, power: JString<'caller>, safe_reason: JString<'caller>),
    |env| Ok(RadioCompletion::Adapter(adapter_snapshot(
        env,
        &availability,
        &authorization,
        &power,
        &safe_reason,
        "mobile.complete.adapter"
    )?))
);

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteDiscovered,
    "mobile.complete.discovered",
    (levels: JIntArray<'caller>, uuids: JObjectArray<'caller, JString<'caller>>, occurrences: JLongArray<'caller>, properties: JIntArray<'caller>),
    |env| {
        const OP: &str = "mobile.complete.discovered";
        let levels = read_ints(env, &levels)?;
        let uuids = read_required_texts(env, &uuids, OP)?;
        let occurrences = read_longs(env, &occurrences)?;
        let properties = read_ints(env, &properties)?;
        discovery_tree(&levels, &uuids, &occurrences, &properties)
            .map(RadioCompletion::Discovered)
            .map_err(|detail| invalid(OP, detail))
    }
);

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteNotifyEnabled,
    "mobile.complete.notify-enabled",
    (delivery: JString<'caller>),
    |env| {
        const OP: &str = "mobile.complete.notify-enabled";
        let delivery = match read_text(env, &delivery, OP)?.as_str() {
            "notification" => ObservedDelivery::Notification,
            "indication" => ObservedDelivery::Indication,
            "unknown" => ObservedDelivery::Unknown,
            _ => return Err(invalid(OP, "delivery must be notification, indication or unknown")),
        };
        Ok(RadioCompletion::NotifyEnabled(delivery))
    }
);

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteMtu,
    "mobile.complete.mtu",
    (mtu: jint),
    |_env| {
        const OP: &str = "mobile.complete.mtu";
        if mtu == 0 {
            return Ok(RadioCompletion::Mtu(None));
        }
        let mtu = u16::try_from(mtu).ok().filter(|mtu| (23..=517).contains(mtu));
        mtu.map(|mtu| RadioCompletion::Mtu(Some(mtu)))
            .ok_or_else(|| invalid(OP, "mtu must be 0 or 23..=517"))
    }
);

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteWriteLimits,
    "mobile.complete.write-limits",
    (with_response: jint, without_response: jint),
    |_env| {
        const OP: &str = "mobile.complete.write-limits";
        let limit = |value: jint| u16::try_from(value).ok().filter(|limit| *limit > 0);
        match (limit(with_response), limit(without_response)) {
            (Some(with_response), Some(without_response)) => {
                Ok(RadioCompletion::WriteLimits(ubm_mobile::WriteLimits {
                    with_response,
                    without_response,
                }))
            }
            _ => Err(invalid(OP, "write limits must be 1..=65535 bytes")),
        }
    }
);

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteRssi,
    "mobile.complete.rssi",
    (rssi: jint),
    |_env| i8::try_from(rssi)
        .map(|rssi| RadioCompletion::Rssi(i16::from(rssi)))
        .map_err(|_| invalid("mobile.complete.rssi", "rssi out of range"))
);

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteAccepted,
    "mobile.complete.accepted",
    (accepted: jboolean),
    |_env| Ok(RadioCompletion::Accepted(accepted))
);

fn phy(
    env: &mut Env,
    value: &JString,
    operation: &'static str,
) -> MobileResult<Option<ubm_mobile::Phy>> {
    read_opt_text(env, value, operation)?
        .map(|text| match text.as_str() {
            "le-1m" => Ok(ubm_mobile::Phy::Le1m),
            "le-2m" => Ok(ubm_mobile::Phy::Le2m),
            "le-coded" => Ok(ubm_mobile::Phy::LeCoded),
            _ => Err(invalid(operation, "phy must be le-1m, le-2m or le-coded")),
        })
        .transpose()
}

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompletePhy,
    "mobile.complete.phy",
    (tx: JString<'caller>, rx: JString<'caller>),
    |env| {
        const OP: &str = "mobile.complete.phy";
        match (phy(env, &tx, OP)?, phy(env, &rx, OP)?) {
            (Some(tx), Some(rx)) => Ok(RadioCompletion::Phy(PhyObservation { tx, rx })),
            _ => Err(invalid(OP, "tx and rx are required")),
        }
    }
);

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompletePhyRequest,
    "mobile.complete.phy-request",
    (accepted: jboolean, tx: JString<'caller>, rx: JString<'caller>),
    |env| {
        const OP: &str = "mobile.complete.phy-request";
        let observation = match (phy(env, &tx, OP)?, phy(env, &rx, OP)?) {
            (Some(tx), Some(rx)) => Some(PhyObservation { tx, rx }),
            (None, None) => None,
            _ => return Err(invalid(OP, "tx and rx are both set or both null")),
        };
        Ok(RadioCompletion::PhyRequest { accepted, observation })
    }
);

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteSecurity,
    "mobile.complete.security",
    (bond: JString<'caller>, encryption: JString<'caller>, authentication: JString<'caller>, secure_connections: JString<'caller>, pairing_possible: jint),
    |env| Ok(RadioCompletion::Security(security_state(
        env,
        &bond,
        &encryption,
        &authentication,
        &secure_connections,
        pairing_possible,
        "mobile.complete.security"
    )?))
);

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteBondedPeers,
    "mobile.complete.bonded-peers",
    (peer_ids: JObjectArray<'caller, JString<'caller>>, names: JObjectArray<'caller, JString<'caller>>),
    |env| {
        const OP: &str = "mobile.complete.bonded-peers";
        let peer_ids = read_required_texts(env, &peer_ids, OP)?;
        let names = read_texts(env, &names, OP)?;
        same_length(OP, &[peer_ids.len(), names.len()])?;
        Ok(RadioCompletion::BondedPeers(
            peer_ids
                .into_iter()
                .zip(names)
                .map(|(peer_id, name)| BondedPeer { peer_id, name })
                .collect(),
        ))
    }
);

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteLease,
    "mobile.complete.lease",
    (lease_id: JString<'caller>),
    |env| Ok(RadioCompletion::Lease(read_text(env, &lease_id, "mobile.complete.lease")?))
);

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteCompanion,
    "mobile.complete.companion",
    (association_id: jlong, peer_id: JString<'caller>, display_name: JString<'caller>),
    |env| {
        const OP: &str = "mobile.complete.companion";
        Ok(RadioCompletion::Companion {
            association_id,
            peer_id: read_opt_text(env, &peer_id, OP)?,
            display_name: read_opt_text(env, &display_name, OP)?,
        })
    }
);

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteClosed,
    "mobile.complete.closed",
    (peer_ids: JObjectArray<'caller, JString<'caller>>, service_uuids: JObjectArray<'caller, JString<'caller>>, service_occurrences: JLongArray<'caller>, characteristic_uuids: JObjectArray<'caller, JString<'caller>>, characteristic_occurrences: JLongArray<'caller>, details: JObjectArray<'caller, JString<'caller>>),
    |env| {
        const OP: &str = "mobile.complete.closed";
        let peer_ids = read_required_texts(env, &peer_ids, OP)?;
        let service_uuids = read_required_texts(env, &service_uuids, OP)?;
        let service_occurrences = read_longs(env, &service_occurrences)?;
        let characteristic_uuids = read_required_texts(env, &characteristic_uuids, OP)?;
        let characteristic_occurrences = read_longs(env, &characteristic_occurrences)?;
        let details = read_required_texts(env, &details, OP)?;
        same_length(
            OP,
            &[
                peer_ids.len(),
                service_uuids.len(),
                service_occurrences.len(),
                characteristic_uuids.len(),
                characteristic_occurrences.len(),
                details.len(),
            ],
        )?;
        let mut failures = Vec::with_capacity(peer_ids.len());
        for index in 0..peer_ids.len() {
            failures.push(CloseFailure {
                instance: Instance {
                    peer_id: peer_ids[index].clone(),
                    service_uuid: service_uuids[index].clone(),
                    service_occurrence: to_u64(service_occurrences[index], OP)?,
                    characteristic_uuid: characteristic_uuids[index].clone(),
                    characteristic_occurrence: to_u64(characteristic_occurrences[index], OP)?,
                },
                detail: details[index].clone(),
            });
        }
        Ok(RadioCompletion::Closed(failures))
    }
);

completion_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeCompleteFailure,
    "mobile.complete.failure",
    (kind: JString<'caller>, gatt_status: jint, detail: JString<'caller>, dispatched: jboolean, native_code: JString<'caller>),
    |env| {
        const OP: &str = "mobile.complete.failure";
        let kind = read_text(env, &kind, OP)?;
        let native_name = read_opt_text(env, &native_code, OP)?.filter(|code| !code.is_empty());
        Ok(RadioCompletion::Failed(PlatformFailure {
            kind: parse_enum(&kind, FailureKind::parse, OP)?,
            gatt_status: optional_int(gatt_status),
            // Android's identity is its GATT status; it has no NSError.
            native_domain: None,
            native_code: None,
            native_name,
            detail: read_opt_text(env, &detail, OP)?.unwrap_or_default(),
            dispatched,
        }))
    }
);

// -- natives: ingress ------------------------------------------------------

macro_rules! ingress_native {
    ($name:ident, $op:literal, ($($param:ident : $ty:ty),*), |$env:ident| $body:expr) => {
        // See `completion_native!`: the closure scopes `?` to the fact.
        #[allow(clippy::redundant_closure_call)]
        #[unsafe(no_mangle)]
        pub extern "system" fn $name<'caller>(
            mut unowned_env: EnvUnowned<'caller>,
            _class: JClass<'caller>,
            $($param: $ty),*
        ) -> jint {
            unowned_env
                .with_env(|$env| -> MobileResult<jint> {
                    let built: MobileResult<RadioIngress> = (|| $body)();
                    Ok(match built {
                        Ok(ingress) => ingest(ingress),
                        // An unreadable fact is counted as a drop of its
                        // class and reported, never silently discarded.
                        Err(error) => {
                            eprintln!("ubm-mobile jni [{}]: {error:?}", $op);
                            ingest(RadioIngress::Dropped {
                                class: ingress_class_of($op),
                                detail: format!("unreadable {}", $op),
                            })
                        }
                    })
                })
                .resolve_with::<ThrowMobile, _>(|| $op)
        }
    };
}

fn ingress_class_of(operation: &str) -> IngressClass {
    match operation {
        "mobile.ingest.advertisement" => IngressClass::Advertisement,
        "mobile.ingest.notification" => IngressClass::Notification,
        _ => IngressClass::Control,
    }
}

ingress_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeIngestAdvertisement,
    "mobile.ingest.advertisement",
    (peer_id: JString<'caller>, address: JString<'caller>, local_name: JString<'caller>, rssi: jint, tx_power: jint, service_uuids: JObjectArray<'caller, JString<'caller>>, company_ids: JIntArray<'caller>, manufacturer_payloads: JObjectArray<'caller, JByteArray<'caller>>, service_data_uuids: JObjectArray<'caller, JString<'caller>>, service_data_payloads: JObjectArray<'caller, JByteArray<'caller>>, connectable: jint, solicited_service_uuids: JObjectArray<'caller, JString<'caller>>, overflow_service_uuids: JObjectArray<'caller, JString<'caller>>, appearance: jint, raw_record: JByteArray<'caller>),
    |env| {
        const OP: &str = "mobile.ingest.advertisement";
        let company_ids = read_ints(env, &company_ids)?;
        let manufacturer_payloads = read_byte_arrays(env, &manufacturer_payloads, OP)?;
        same_length(OP, &[company_ids.len(), manufacturer_payloads.len()])?;
        let service_data_uuids = read_required_texts(env, &service_data_uuids, OP)?;
        let service_data_payloads = read_byte_arrays(env, &service_data_payloads, OP)?;
        same_length(OP, &[service_data_uuids.len(), service_data_payloads.len()])?;
        Ok(RadioIngress::Advertisement(Advertisement {
            peer_id: read_text(env, &peer_id, OP)?,
            address: read_opt_text(env, &address, OP)?,
            local_name: read_opt_text(env, &local_name, OP)?,
            rssi: optional_i16(rssi, OP)?,
            tx_power_level: optional_i16(tx_power, OP)?,
            service_uuids: read_required_texts(env, &service_uuids, OP)?,
            manufacturer_data: company_ids
                .into_iter()
                .zip(manufacturer_payloads)
                .map(|(company_id, payload)| {
                    u16::try_from(company_id)
                        .map(|company_id| ManufacturerData { company_id, payload })
                        .map_err(|_| invalid(OP, "company id out of range"))
                })
                .collect::<MobileResult<Vec<_>>>()?,
            service_data: service_data_uuids
                .into_iter()
                .zip(service_data_payloads)
                .map(|(uuid, payload)| ServiceData { uuid, payload })
                .collect(),
            connectable: match connectable {
                -1 => None,
                0 => Some(false),
                1 => Some(true),
                _ => return Err(invalid(OP, "connectable must be -1, 0 or 1")),
            },
            solicited_service_uuids: read_opt_texts(env, &solicited_service_uuids, OP)?,
            overflow_service_uuids: read_opt_texts(env, &overflow_service_uuids, OP)?,
            appearance: optional_int(appearance)
                .map(|value| {
                    u16::try_from(value).map_err(|_| invalid(OP, "appearance out of range"))
                })
                .transpose()?,
            raw_record: if raw_record.as_raw().is_null() {
                None
            } else {
                Some(read_bytes(env, &raw_record, OP)?)
            },
        }))
    }
);

ingress_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeIngestConnection,
    "mobile.ingest.connection",
    (peer_id: JString<'caller>, connected: jboolean, status: jint),
    |env| Ok(RadioIngress::Connection {
        peer_id: read_text(env, &peer_id, "mobile.ingest.connection")?,
        connected,
        status: optional_int(status),
    })
);

ingress_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeIngestServicesChanged,
    "mobile.ingest.services-changed",
    (peer_id: JString<'caller>),
    |env| Ok(RadioIngress::ServicesChanged {
        peer_id: read_text(env, &peer_id, "mobile.ingest.services-changed")?,
    })
);

ingress_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeIngestNotification,
    "mobile.ingest.notification",
    (peer_id: JString<'caller>, service_uuid: JString<'caller>, service_occurrence: jlong, characteristic_uuid: JString<'caller>, characteristic_occurrence: jlong, epoch: jlong, value: JByteArray<'caller>),
    |env| {
        const OP: &str = "mobile.ingest.notification";
        Ok(RadioIngress::Notification {
            instance: instance(
                env,
                &peer_id,
                &service_uuid,
                service_occurrence,
                &characteristic_uuid,
                characteristic_occurrence,
                OP,
            )?,
            epoch: to_u64(epoch, OP)?,
            value: read_bytes(env, &value, OP)?,
        })
    }
);

ingress_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeIngestAdapterState,
    "mobile.ingest.adapter-state",
    (availability: JString<'caller>, authorization: JString<'caller>, power: JString<'caller>, safe_reason: JString<'caller>),
    |env| Ok(RadioIngress::AdapterState(adapter_snapshot(
        env,
        &availability,
        &authorization,
        &power,
        &safe_reason,
        "mobile.ingest.adapter-state"
    )?))
);

ingress_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeIngestScanFailed,
    "mobile.ingest.scan-failed",
    (detail: JString<'caller>),
    |env| Ok(RadioIngress::ScanFailed {
        detail: read_opt_text(env, &detail, "mobile.ingest.scan-failed")?.unwrap_or_default(),
    })
);

ingress_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeIngestSecurity,
    "mobile.ingest.security",
    (peer_id: JString<'caller>, bond: JString<'caller>, encryption: JString<'caller>, authentication: JString<'caller>, secure_connections: JString<'caller>, pairing_possible: jint),
    |env| {
        const OP: &str = "mobile.ingest.security";
        Ok(RadioIngress::SecurityChanged {
            peer_id: read_text(env, &peer_id, OP)?,
            state: security_state(
                env,
                &bond,
                &encryption,
                &authentication,
                &secure_connections,
                pairing_possible,
                OP,
            )?,
        })
    }
);

ingress_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeIngestRestored,
    "mobile.ingest.restored",
    (peer_ids: JObjectArray<'caller, JString<'caller>>, names: JObjectArray<'caller, JString<'caller>>, connected: JBooleanArray<'caller>),
    |env| {
        const OP: &str = "mobile.ingest.restored";
        let peer_ids = read_required_texts(env, &peer_ids, OP)?;
        let names = read_texts(env, &names, OP)?;
        let connected = read_bools(env, &connected)?;
        same_length(OP, &[peer_ids.len(), names.len(), connected.len()])?;
        Ok(RadioIngress::Restored {
            peers: peer_ids
                .into_iter()
                .zip(names)
                .zip(connected)
                .map(|((peer_id, name), connected)| RestoredPeer { peer_id, name, connected })
                .collect(),
        })
    }
);

ingress_native!(
    Java_com_ubm_core_MobileCoreBridge_nativeIngestDropped,
    "mobile.ingest.dropped",
    (ingress_class: JString<'caller>, detail: JString<'caller>),
    |env| {
        const OP: &str = "mobile.ingest.dropped";
        let class = match read_text(env, &ingress_class, OP)?.as_str() {
            "advertisement" => IngressClass::Advertisement,
            "notification" => IngressClass::Notification,
            "control" => IngressClass::Control,
            _ => return Err(invalid(OP, "class must be advertisement, notification or control")),
        };
        Ok(RadioIngress::Dropped {
            class,
            detail: read_opt_text(env, &detail, OP)?.unwrap_or_default(),
        })
    }
);

#[cfg(test)]
mod tests {
    use super::*;
    use ubm_mobile::{Instance, ScanRequest};

    fn hr() -> Instance {
        Instance {
            peer_id: "p".to_owned(),
            service_uuid: "s".to_owned(),
            service_occurrence: 0,
            characteristic_uuid: "c".to_owned(),
            characteristic_occurrence: 1,
        }
    }

    #[test]
    fn requests_map_to_typed_host_calls() {
        let (method, args) = request_call(&RadioRequest::EnableNotifications {
            id: 7,
            instance: hr(),
            epoch: 3,
            requested: Some(DeliveryMode::Indication),
            preferred: None,
        });
        assert_eq!(method, "enableNotifications");
        assert_eq!(
            signature(&args),
            "(JLjava/lang/String;Ljava/lang/String;JLjava/lang/String;JJLjava/lang/String;Ljava/lang/String;)V"
        );
        assert_eq!(args[7], Arg::Str(Some("indication".to_owned())));
        assert_eq!(args[8], Arg::Str(None));
        let (method, args) = request_call(&RadioRequest::StartScan {
            id: 1,
            scan: ScanRequest::default(),
        });
        assert_eq!(method, "startScan");
        assert_eq!(
            signature(&args),
            "(J[Ljava/lang/String;[Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;I)V"
        );
        assert_eq!(args[5], Arg::Int(-1));
        let (method, args) = request_call(&RadioRequest::Connect {
            id: 2,
            peer_id: "p".to_owned(),
            auto_connect: false,
            preferred_phy: vec![ubm_mobile::Phy::Le2m, ubm_mobile::Phy::LeCoded],
        });
        assert_eq!(method, "connect");
        assert_eq!(
            signature(&args),
            "(JLjava/lang/String;Z[Ljava/lang/String;)V"
        );
        assert_eq!(
            args[3],
            Arg::Strings(vec!["le-2m".to_owned(), "le-coded".to_owned()])
        );
        let (method, args) = request_call(&RadioRequest::ObservePresence {
            id: 11,
            peer_id: "AA:BB:CC:DD:EE:FF".to_owned(),
        });
        assert_eq!(method, "observePresence");
        assert_eq!(signature(&args), "(JLjava/lang/String;)V");
        let (method, args) = request_call(&RadioRequest::StopPresence {
            id: 12,
            peer_id: "AA:BB:CC:DD:EE:FF".to_owned(),
        });
        assert_eq!(method, "unobservePresence");
        assert_eq!(signature(&args), "(JLjava/lang/String;)V");
    }

    #[test]
    fn every_request_signature_matches_the_java_interface() {
        let java =
            include_str!("../../../android/src/main/java/com/ubm/core/MobileCoreBridge.java");
        let requests = [
            RadioRequest::AdapterState { id: 1 },
            RadioRequest::StopScan { id: 1 },
            RadioRequest::Close { id: 1 },
            RadioRequest::BondedPeers { id: 1 },
            RadioRequest::Read {
                id: 1,
                instance: hr(),
            },
            RadioRequest::DisableNotifications {
                id: 1,
                instance: hr(),
            },
            RadioRequest::Connect {
                id: 1,
                peer_id: "p".to_owned(),
                auto_connect: false,
                preferred_phy: Vec::new(),
            },
        ];
        for request in requests {
            let (method, _) = request_call(&request);
            assert!(
                java.contains(&format!("void {method}(long requestId")),
                "{method}"
            );
        }
    }

    #[test]
    fn discovery_tree_rebuilds_pre_order() {
        let tree = discovery_tree(
            &[0, 1, 2],
            &["180d".to_owned(), "2a37".to_owned(), "2902".to_owned()],
            &[0, 0, 0],
            &[0, 0x10, 0],
        );
        let tree = tree.unwrap_or_default();
        assert_eq!(tree.len(), 1);
        assert!(tree[0].characteristics[0].properties.notify);
        assert_eq!(tree[0].characteristics[0].descriptors.len(), 1);
        assert!(discovery_tree(&[1], &["x".to_owned()], &[0], &[0]).is_err());
        assert!(discovery_tree(&[0], &["x".to_owned()], &[-1], &[0]).is_err());
    }

    #[test]
    fn java_copies_are_identical() {
        let shipped =
            include_str!("../../../android/src/main/java/com/ubm/core/MobileCoreBridge.java");
        let probe = include_str!("../java/com/ubm/core/MobileCoreBridge.java");
        assert_eq!(shipped, probe, "MobileCoreBridge.java copies drifted");
    }

    #[test]
    fn mobile_surface_has_no_staged_path() {
        let source = include_str!("mobile.rs");
        let body = source.split("#[cfg(test)]").next().unwrap_or(source);
        for forbidden in [
            "ubm_fake_radio",
            "StagedDriver",
            "core_backend",
            "EchoBridge",
        ] {
            assert!(
                !body.contains(forbidden),
                "{forbidden} reachable from the mobile surface"
            );
        }
    }
}
