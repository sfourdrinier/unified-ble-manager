//! Linux-only: the kernel's Bluetooth management control channel
//! (`AF_BLUETOOTH` / `BTPROTO_HCI` bound to `HCI_CHANNEL_CONTROL`) and the
//! `mgmt-legacy` advertising instance the sim owns on it.
//!
//! No maintained crate covers this surface (the `btmgmt` crates are
//! unreleased alphas), so the socket is a few `libc` calls: `socket`,
//! `bind`, `write`, `poll`, `read`. Packets are built and parsed by the pure
//! `crate::mgmt` module.
//!
//! Privilege: the kernel marks a control socket trusted only when the
//! binding process holds `CAP_NET_ADMIN` (net/bluetooth/hci_sock.c); an
//! untrusted socket is refused every command used here with `Permission
//! Denied (0x14)`. The capability is checked before the socket is opened and
//! the refusal is mapped to the same message, naming the `setcap` command.
//!
//! Cleanup: the instance is removed on stop, on drop, and from a panic hook;
//! a record in the runtime directory lets the next start remove an instance
//! left by a run that could not clean up (SIGKILL).

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::linux_advertising::{self, InstanceRecord, StaleAction};
use crate::mgmt::{self, Event};

/// `BTPROTO_HCI` (include/net/bluetooth/bluetooth.h).
const BTPROTO_HCI: libc::c_int = 1;
/// `HCI_CHANNEL_CONTROL` (include/net/bluetooth/hci_sock.h).
const HCI_CHANNEL_CONTROL: u16 = 3;
/// `HCI_DEV_NONE`: control sockets bind to no device.
const HCI_DEV_NONE: u16 = 0xFFFF;
/// Largest possible event: header plus a u16 parameter length.
const MAX_EVENT_LEN: usize = mgmt::HEADER_LEN + u16::MAX as usize;
/// A command the kernel has not answered in this long is reported as such.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// `struct sockaddr_hci` (include/net/bluetooth/hci_sock.h).
#[repr(C)]
struct SockaddrHci {
    hci_family: libc::sa_family_t,
    hci_dev: u16,
    hci_channel: u16,
}

/// A failed MGMT exchange: a kernel status, or a transport fault.
#[derive(Debug)]
pub enum MgmtFailure {
    Status { opcode: u16, status: u8 },
    Transport(String),
}

impl MgmtFailure {
    fn describe(&self) -> String {
        match self {
            Self::Status { opcode, status } => format!(
                "MGMT opcode 0x{opcode:04x} answered {} (0x{status:02x})",
                mgmt::status_name(*status)
            ),
            Self::Transport(message) => message.clone(),
        }
    }
}

/// How an extended-advertising start attempt ended: the kernel refused
/// `0x0054` itself (fall back to legacy, loudly), or something else failed.
#[derive(Debug)]
enum ExtStartError {
    Refused(String),
    Failed(String),
}

/// An open, bound control socket.
struct MgmtSocket {
    fd: OwnedFd,
}

impl MgmtSocket {
    fn open() -> Result<Self, String> {
        // SAFETY: plain socket(2); the result is checked before use.
        let raw = unsafe {
            libc::socket(
                libc::AF_BLUETOOTH,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                BTPROTO_HCI,
            )
        };
        if raw < 0 {
            return Err(format!(
                "open Bluetooth management socket: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: `raw` is a fresh descriptor owned by nothing else.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let address = SockaddrHci {
            hci_family: libc::AF_BLUETOOTH as libc::sa_family_t,
            hci_dev: HCI_DEV_NONE,
            hci_channel: HCI_CHANNEL_CONTROL,
        };
        // SAFETY: `address` is a valid sockaddr_hci for the stated length.
        let bound = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                std::ptr::addr_of!(address).cast::<libc::sockaddr>(),
                std::mem::size_of::<SockaddrHci>() as libc::socklen_t,
            )
        };
        if bound < 0 {
            return Err(format!(
                "bind Bluetooth management control channel: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Self { fd })
    }

    fn send(&self, packet: &[u8]) -> Result<(), MgmtFailure> {
        // SAFETY: `packet` is a valid buffer of the given length.
        let written =
            unsafe { libc::write(self.fd.as_raw_fd(), packet.as_ptr().cast(), packet.len()) };
        if written < 0 {
            return Err(MgmtFailure::Transport(format!(
                "write MGMT command: {}",
                std::io::Error::last_os_error()
            )));
        }
        if written as usize != packet.len() {
            return Err(MgmtFailure::Transport(format!(
                "short MGMT write: {written} of {} bytes",
                packet.len()
            )));
        }
        Ok(())
    }

    /// Next event within `wait`, or None when nothing arrived in time.
    fn next_event(&self, wait: Duration) -> Result<Option<Event>, MgmtFailure> {
        let mut poll_fd = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let millis = libc::c_int::try_from(wait.as_millis()).unwrap_or(libc::c_int::MAX);
        // SAFETY: one valid pollfd.
        let ready = unsafe { libc::poll(&mut poll_fd, 1, millis) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                return Ok(None);
            }
            return Err(MgmtFailure::Transport(format!("poll MGMT socket: {error}")));
        }
        if ready == 0 {
            return Ok(None);
        }
        let mut buffer = vec![0u8; MAX_EVENT_LEN];
        // SAFETY: `buffer` is writable for its full length.
        let read = unsafe {
            libc::read(
                self.fd.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
            )
        };
        if read < 0 {
            return Err(MgmtFailure::Transport(format!(
                "read MGMT event: {}",
                std::io::Error::last_os_error()
            )));
        }
        buffer.truncate(read as usize);
        mgmt::parse_event(&buffer)
            .map(Some)
            .map_err(MgmtFailure::Transport)
    }

    /// Sends one command and waits for its Command Complete / Command
    /// Status. Every other event read meanwhile goes to `observe`.
    fn command(
        &self,
        index: u16,
        opcode: u16,
        params: &[u8],
        observe: &mut dyn FnMut(&Event),
    ) -> Result<Vec<u8>, MgmtFailure> {
        let packet = mgmt::command(opcode, index, params).map_err(MgmtFailure::Transport)?;
        self.send(&packet)?;
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(MgmtFailure::Transport(format!(
                    "MGMT opcode 0x{opcode:04x} unanswered after {COMMAND_TIMEOUT:?}"
                )));
            }
            match self.next_event(left)? {
                Some(Event::CommandComplete {
                    index: event_index,
                    opcode: event_opcode,
                    status,
                    data,
                }) if event_index == index && event_opcode == opcode => {
                    return if status == mgmt::STATUS_SUCCESS {
                        Ok(data)
                    } else {
                        Err(MgmtFailure::Status { opcode, status })
                    };
                }
                Some(Event::CommandStatus {
                    index: event_index,
                    opcode: event_opcode,
                    status,
                }) if event_index == index && event_opcode == opcode => {
                    return Err(MgmtFailure::Status { opcode, status });
                }
                Some(other) => observe(&other),
                None => {}
            }
        }
    }
}

/// The instance this process must remove if it dies: read by `stop`, `Drop`
/// and the panic hook, so exactly one of them removes it.
struct Registered {
    index: u16,
    instance: u8,
    record: PathBuf,
}

static REGISTERED: Mutex<Option<Registered>> = Mutex::new(None);
static PANIC_HOOK: Once = Once::new();

fn registered() -> std::sync::MutexGuard<'static, Option<Registered>> {
    REGISTERED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Removes the instance when the main thread panics. The simulator loop runs
/// on the main thread (`block_on`), so its panic ends the process; unwinding
/// would also reach `Drop`, but the hook does not depend on unwinding. A
/// panic in a side task (control port, driver link) leaves the process — and
/// its advertisement — running, so it is left alone here.
fn install_panic_hook() {
    PANIC_HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            previous(info);
            if std::thread::current().name() != Some("main") {
                return;
            }
            let Some(owned) = registered().take() else {
                return;
            };
            let outcome = MgmtSocket::open()
                .map_err(MgmtFailure::Transport)
                .and_then(|socket| {
                    socket.command(
                        owned.index,
                        mgmt::OP_REMOVE_ADVERTISING,
                        &mgmt::remove_advertising_params(owned.instance),
                        &mut |_| {},
                    )
                });
            match outcome {
                Ok(_) => {
                    remove_record(&owned.record);
                    eprintln!(
                        "h10-sim: panic cleanup removed advertising instance {} on hci{}",
                        owned.instance, owned.index
                    );
                }
                Err(failure) => eprintln!(
                    "h10-sim: panic cleanup could NOT remove advertising instance {} on hci{}: {} \
                     (the next start removes it; or `sudo btmgmt rm-adv {}`)",
                    owned.instance,
                    owned.index,
                    failure.describe(),
                    owned.instance
                ),
            }
        }));
    });
}

fn remove_record(path: &Path) {
    if let Err(error) = std::fs::remove_file(path) {
        if error.kind() != std::io::ErrorKind::NotFound {
            eprintln!(
                "h10-sim: cannot remove instance record {}: {error}",
                path.display()
            );
        }
    }
}

fn record_path(index: u16) -> PathBuf {
    let directory = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    directory.join(format!("h10-sim-mgmt-hci{index}.instance"))
}

/// Refuses early, with the setcap command, when `CAP_NET_ADMIN` is missing.
pub fn require_net_admin() -> Result<(), String> {
    let status = std::fs::read_to_string("/proc/self/status")
        .map_err(|error| format!("read /proc/self/status: {error}"))?;
    if linux_advertising::effective_has_net_admin(&status)? {
        Ok(())
    } else {
        Err(linux_advertising::missing_capability_message(&exe_path()))
    }
}

fn exe_path() -> String {
    std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|error| format!("<path of h10-sim: {error}>"))
}

fn failure_message(context: &str, failure: MgmtFailure) -> String {
    match failure {
        MgmtFailure::Status {
            status: mgmt::STATUS_PERMISSION_DENIED,
            ..
        } => format!(
            "{context}: {}. {}",
            failure.describe(),
            linux_advertising::missing_capability_message(&exe_path())
        ),
        other => format!("{context}: {}", other.describe()),
    }
}

/// Which MGMT commands put the instance on the air.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdvMethod {
    /// `MGMT_OP_ADD_EXT_ADV_PARAMS` (with the H10's ~1 s interval) plus
    /// `MGMT_OP_ADD_EXT_ADV_DATA`, both exactly sized.
    Ext,
    /// `MGMT_OP_ADD_ADVERTISING`: the kernel refused 0x0054, reported loudly.
    LegacyFallback,
}

impl AdvMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ext => "ext-0x0054-0x0055",
            Self::LegacyFallback => "legacy-0x003e-fallback",
        }
    }
}

/// What `start` installed on the controller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvStart {
    pub instance: u8,
    pub method: AdvMethod,
    /// Why the legacy fallback was used (`None` on the ext path).
    pub fallback_reason: Option<String>,
}

/// The advertising instance this sim owns on one controller.
pub struct MgmtAdvertiser {
    socket: MgmtSocket,
    index: u16,
    boot_id: String,
    record: PathBuf,
    active: Option<u8>,
    max_instances: u8,
    method: Option<AdvMethod>,
}

impl MgmtAdvertiser {
    /// Opens the control channel for `index`, reads the controller's
    /// advertising features, and removes an instance a previous run of this
    /// sim left behind in this boot. Returns what it found, for the log.
    pub fn open(index: u16) -> Result<(Self, Value), String> {
        require_net_admin()?;
        let socket = MgmtSocket::open()?;
        let mut advertiser = Self {
            socket,
            index,
            boot_id: linux_advertising::boot_id()?,
            record: record_path(index),
            active: None,
            max_instances: 0,
            method: None,
        };
        let features = advertiser.read_features()?;
        advertiser.max_instances = features.max_instances;
        let stale = advertiser.clean_stale(&features.instances)?;
        let found = json!({
            "index": index,
            "maxInstances": features.max_instances,
            "listedInstances": features.instances,
            "record": advertiser.record.display().to_string(),
            "stale": stale,
        });
        Ok((advertiser, found))
    }

    fn observe(active: &mut Option<u8>, index: u16, record: &Path, event: &Event) {
        let gone = match event {
            Event::AdvertisingRemoved {
                index: event_index,
                instance,
            } => *event_index == index && Some(*instance) == *active,
            Event::IndexRemoved { index: event_index } => *event_index == index,
            _ => false,
        };
        if gone {
            eprintln!(
                "h10-sim: the kernel reports advertising instance {:?} on hci{index} removed \
                 by someone else ({event:?})",
                active
            );
            *active = None;
            registered().take();
            remove_record(record);
        }
    }

    fn command(&mut self, opcode: u16, params: &[u8]) -> Result<Vec<u8>, MgmtFailure> {
        let Self {
            socket,
            index,
            record,
            active,
            ..
        } = self;
        let index = *index;
        socket.command(index, opcode, params, &mut |event| {
            Self::observe(active, index, record, event)
        })
    }

    fn read_features(&mut self) -> Result<mgmt::AdvFeatures, String> {
        let data = self
            .command(mgmt::OP_READ_ADV_FEATURES, &[])
            .map_err(|failure| failure_message("Read Advertising Features", failure))?;
        mgmt::parse_adv_features(&data)
    }

    fn clean_stale(&mut self, listed: &[u8]) -> Result<Value, String> {
        let text = match std::fs::read_to_string(&self.record) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Value::Null);
            }
            Err(error) => {
                return Err(format!(
                    "read instance record {}: {error}",
                    self.record.display()
                ))
            }
        };
        let record = match InstanceRecord::parse(&text) {
            Ok(record) => record,
            Err(error) => {
                eprintln!(
                    "h10-sim: discarding unreadable instance record {}: {error}",
                    self.record.display()
                );
                remove_record(&self.record);
                return Ok(json!({"discarded": "unreadable"}));
            }
        };
        match linux_advertising::stale_action(&record, &self.boot_id, self.index, listed) {
            StaleAction::Remove(instance) => {
                self.command(
                    mgmt::OP_REMOVE_ADVERTISING,
                    &mgmt::remove_advertising_params(instance),
                )
                .map_err(|failure| {
                    failure_message(
                        &format!("remove stale advertising instance {instance}"),
                        failure,
                    )
                })?;
                remove_record(&self.record);
                Ok(json!({"removedInstance": instance}))
            }
            StaleAction::DiscardRecord => {
                remove_record(&self.record);
                Ok(json!({"discardedRecord": record.instance}))
            }
        }
    }

    /// Adds the H10 advertisement on a free instance. Replaces an instance
    /// this advertiser already holds. Sends the extended `0x0054` params
    /// (with the H10's interval) plus `0x0055` data first; only when the
    /// kernel refuses `0x0054` itself does it fall back to the legacy
    /// `0x003E`, and that fallback is reported — in the returned
    /// [`AdvStart`], on stderr, and in the advertising detail — never silent.
    pub fn start(
        &mut self,
        name: &str,
        uuids16: &[u16],
        mfr: Option<(u16, &[u8])>,
    ) -> Result<AdvStart, String> {
        self.stop()?;
        let features = self.read_features()?;
        let instance = mgmt::pick_instance(&features)?;
        let (method, fallback_reason) = match self.start_ext(instance, name, uuids16, mfr) {
            Ok(()) => (AdvMethod::Ext, None),
            Err(ExtStartError::Refused(reason)) => {
                let message = format!(
                    "MGMT_OP_ADD_EXT_ADV_PARAMS (0x0054) refused on hci{}: {reason}; \
                     falling back to MGMT_OP_ADD_ADVERTISING (0x003E) without an explicit \
                     advertising interval",
                    self.index
                );
                eprintln!("h10-sim: {message}");
                self.start_legacy(instance, name, uuids16, mfr)?;
                (AdvMethod::LegacyFallback, Some(message))
            }
            Err(ExtStartError::Failed(message)) => return Err(message),
        };
        self.active = Some(instance);
        self.method = Some(method);
        *registered() = Some(Registered {
            index: self.index,
            instance,
            record: self.record.clone(),
        });
        install_panic_hook();
        let record = InstanceRecord {
            boot_id: self.boot_id.clone(),
            index: self.index,
            instance,
        };
        std::fs::write(&self.record, record.encode()).map_err(|error| {
            format!(
                "advertising instance {instance} added, but its record {} could not be written \
                 ({error}): a SIGKILL would leave it behind",
                self.record.display()
            )
        })?;
        Ok(AdvStart {
            instance,
            method,
            fallback_reason,
        })
    }

    /// Sends `0x0054` then `0x0055`. A kernel refusal of `0x0054` itself is
    /// `Refused` (the caller falls back); anything else is `Failed`.
    fn start_ext(
        &mut self,
        instance: u8,
        name: &str,
        uuids16: &[u16],
        mfr: Option<(u16, &[u8])>,
    ) -> Result<(), ExtStartError> {
        let params = mgmt::h10_ext_adv_params(instance).map_err(ExtStartError::Failed)?;
        match self.command(
            mgmt::OP_ADD_EXT_ADV_PARAMS,
            &params.params().map_err(ExtStartError::Failed)?,
        ) {
            Ok(data) => {
                let reply =
                    mgmt::parse_add_ext_adv_params_reply(&data).map_err(ExtStartError::Failed)?;
                if reply.instance != instance {
                    return Err(ExtStartError::Failed(format!(
                        "Add Extended Advertising Params asked for instance {instance}, \
                         the kernel answered {:?}",
                        reply.instance
                    )));
                }
            }
            Err(MgmtFailure::Status { opcode, status })
                if opcode == mgmt::OP_ADD_EXT_ADV_PARAMS =>
            {
                return Err(ExtStartError::Refused(format!(
                    "{} (0x{status:02x})",
                    mgmt::status_name(status)
                )));
            }
            Err(failure) => {
                return Err(ExtStartError::Failed(failure_message(
                    &format!("Add Extended Advertising Params instance {instance}"),
                    failure,
                )));
            }
        }
        let data_request =
            mgmt::h10_ext_adv_data(instance, name, uuids16, mfr).map_err(ExtStartError::Failed)?;
        let data = self
            .command(
                mgmt::OP_ADD_EXT_ADV_DATA,
                &data_request.params().map_err(ExtStartError::Failed)?,
            )
            .map_err(|failure| {
                ExtStartError::Failed(failure_message(
                    &format!("Add Extended Advertising Data instance {instance}"),
                    failure,
                ))
            })?;
        let added = mgmt::parse_add_ext_adv_data_reply(&data).map_err(ExtStartError::Failed)?;
        if added != instance {
            // Params accepted but data landed elsewhere: remove the params
            // instance best-effort so no half-configured instance is left,
            // and report both outcomes loudly.
            if let Err(failure) = self.command(
                mgmt::OP_REMOVE_ADVERTISING,
                &mgmt::remove_advertising_params(instance),
            ) {
                eprintln!(
                    "h10-sim: cleanup of half-added ext instance {instance} on hci{} failed: {}",
                    self.index,
                    failure_message("Remove Advertising", failure),
                );
            }
            return Err(ExtStartError::Failed(format!(
                "Add Extended Advertising Data asked for instance {instance}, \
                 the kernel answered {added}"
            )));
        }
        Ok(())
    }

    /// Sends the legacy `0x003E`: no explicit interval, same payload bytes.
    fn start_legacy(
        &mut self,
        instance: u8,
        name: &str,
        uuids16: &[u16],
        mfr: Option<(u16, &[u8])>,
    ) -> Result<(), String> {
        let request = mgmt::h10_add_advertising(instance, name, uuids16, mfr)?;
        let data = self
            .command(mgmt::OP_ADD_ADVERTISING, &request.params()?)
            .map_err(|failure| {
                failure_message(&format!("Add Advertising instance {instance}"), failure)
            })?;
        let added = data.first().copied();
        if added != Some(instance) {
            return Err(format!(
                "Add Advertising asked for instance {instance}, the kernel answered {added:?}"
            ));
        }
        Ok(())
    }

    /// Removes the held instance. Returns the instance removed, or None when
    /// nothing was held (never added, or removed by someone else).
    pub fn stop(&mut self) -> Result<Option<u8>, String> {
        self.drain_events()?;
        let Some(instance) = self.active else {
            return Ok(None);
        };
        let removed = self.command(
            mgmt::OP_REMOVE_ADVERTISING,
            &mgmt::remove_advertising_params(instance),
        );
        match removed {
            Ok(_) => {
                self.active = None;
                registered().take();
                remove_record(&self.record);
                Ok(Some(instance))
            }
            Err(failure) => Err(failure_message(
                &format!("Remove Advertising instance {instance}"),
                failure,
            )),
        }
    }

    /// Whether the held instance is still installed: events the kernel sent
    /// since the last exchange are read first, so a removal by anyone else
    /// is seen.
    pub fn is_active(&mut self) -> Result<bool, String> {
        self.drain_events()?;
        Ok(self.active.is_some())
    }

    fn drain_events(&mut self) -> Result<(), String> {
        loop {
            match self.socket.next_event(Duration::ZERO) {
                Ok(Some(event)) => {
                    Self::observe(&mut self.active, self.index, &self.record, &event)
                }
                Ok(None) => return Ok(()),
                Err(failure) => return Err(failure.describe()),
            }
        }
    }

    pub fn detail(&self) -> Value {
        json!({
            "backend": "mgmt-legacy",
            "index": self.index,
            "instance": self.active,
            "method": self.method.map(AdvMethod::as_str),
            "maxInstances": self.max_instances,
            "record": self.record.display().to_string(),
        })
    }
}

impl Drop for MgmtAdvertiser {
    fn drop(&mut self) {
        if registered().is_none() {
            return;
        }
        match self.stop() {
            Ok(Some(instance)) => eprintln!(
                "h10-sim: removed advertising instance {instance} on hci{} at shutdown",
                self.index
            ),
            Ok(None) => {}
            Err(error) => eprintln!(
                "h10-sim: could NOT remove advertising instance at shutdown: {error} \
                 (the next start removes it; or `sudo btmgmt rm-adv <instance>`)"
            ),
        }
    }
}
