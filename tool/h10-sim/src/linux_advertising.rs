//! `--linux-advertising`: which path puts the advertisement on the air on
//! Linux, plus the pure pieces of both paths (failure diagnosis, capability
//! check, stale-instance record). Compiled and tested on every platform; the
//! radio work lives in `src/bluer_radio.rs` and `src/mgmt_socket.rs`.
//!
//! * `bluez` (default): an `LEAdvertisement1` object registered with
//!   bluetoothd over D-Bus. Needs no privilege.
//! * `mgmt-legacy` (explicit opt-in): the GATT application still goes through
//!   bluetoothd, but the advertisement is added by the sim itself on the
//!   kernel's MGMT control channel with `MGMT_OP_ADD_ADVERTISING` (see
//!   `src/mgmt.rs` for why). Needs `CAP_NET_ADMIN` on the binary; the sim
//!   never acquires it by itself.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

/// The `--linux-advertising` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LinuxAdvertising {
    #[default]
    Bluez,
    MgmtLegacy,
}

impl LinuxAdvertising {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "bluez" => Ok(Self::Bluez),
            "mgmt-legacy" => Ok(Self::MgmtLegacy),
            other => Err(format!(
                "--linux-advertising must be bluez or mgmt-legacy, got {other:?}"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bluez => "bluez",
            Self::MgmtLegacy => "mgmt-legacy",
        }
    }
}

/// Exit status for an advertising failure that restarting cannot fix (the
/// BlueZ/kernel mismatch, a missing capability). `EX_CONFIG` from
/// sysexits.h; the systemd units list it in `RestartPreventExitStatus=` so
/// the service never loops on it — each bluetoothd attempt leaks a kernel
/// advertising instance.
pub const EXIT_ADVERTISING_UNAVAILABLE: u8 = 78;

/// Linux capability number of `CAP_NET_ADMIN` (include/uapi/linux/capability.h).
pub const CAP_NET_ADMIN: u32 = 12;

/// Whether the `CapEff:` line of `/proc/self/status` holds `CAP_NET_ADMIN`.
/// A status text without a parseable `CapEff:` line is an error, never a
/// guess in either direction.
pub fn effective_has_net_admin(proc_status: &str) -> Result<bool, String> {
    let line = proc_status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:"))
        .ok_or_else(|| "no CapEff line in /proc/self/status".to_string())?;
    let mask = u64::from_str_radix(line.trim(), 16)
        .map_err(|error| format!("unparseable CapEff {:?}: {error}", line.trim()))?;
    Ok(mask & (1 << CAP_NET_ADMIN) != 0)
}

/// The refusal for a process without `CAP_NET_ADMIN`: names the privilege,
/// the exact command that grants it to this binary, and how to take it back.
pub fn missing_capability_message(exe: &str) -> String {
    format!(
        "--linux-advertising mgmt-legacy needs CAP_NET_ADMIN to add an advertising instance \
         on the kernel's Bluetooth management socket, and this process does not have it. \
         Grant it to this binary explicitly: `sudo setcap cap_net_admin+ep {exe}` \
         (removed with `sudo setcap -r {exe}`; a rebuild replaces the file and drops it). \
         The sim never escalates by itself; see the README Linux section for what the \
         capability allows."
    )
}

/// Controller index of a BlueZ adapter name (`hci0` -> 0).
pub fn hci_index(adapter_name: &str) -> Result<u16, String> {
    adapter_name
        .strip_prefix("hci")
        .and_then(|digits| digits.parse::<u16>().ok())
        .ok_or_else(|| format!("adapter name {adapter_name:?} is not hciN"))
}

/// What bluetoothd said when an `LEAdvertisement1` registration failed.
pub struct BluezRegistrationFailure<'a> {
    /// The D-Bus error is `org.bluez.Error.Failed`.
    pub failed_kind: bool,
    pub message: &'a str,
    /// `LEAdvertisingManager1` counters read before registering.
    pub instances_before: &'a str,
    /// `uname -r`, or why it could not be read.
    pub kernel_release: &'a str,
}

/// The diagnosis a registration failure carries, and whether it is the known
/// BlueZ/kernel mismatch (permanent: restarting cannot fix it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnosis {
    pub known_mismatch: bool,
    pub text: String,
}

/// bluetoothd answers `org.bluez.Error.Failed: Failed to register
/// advertisement` for any kernel status (src/advertising.c
/// `add_client_complete()`); the status itself only reaches the journal. The
/// diagnosis therefore names the mismatch, the log line that confirms it,
/// and the opt-in workaround, without claiming a status the sim never saw.
pub fn diagnose_bluez_registration(failure: &BluezRegistrationFailure) -> Diagnosis {
    let observed = format!(
        "bluetoothd rejected the LEAdvertisement1 registration: {} \
         (before registering: {}; kernel {})",
        failure.message, failure.instances_before, failure.kernel_release
    );
    let known_mismatch =
        failure.failed_kind && failure.message.contains("Failed to register advertisement");
    if !known_mismatch {
        return Diagnosis {
            known_mismatch,
            text: observed,
        };
    }
    Diagnosis {
        known_mismatch,
        text: format!(
            "{observed}. bluetoothd could not add the advertisement to the kernel. \
             Known BlueZ/kernel mismatch: bluetoothd sends MGMT_OP_ADD_EXT_ADV_DATA (0x0055) \
             sized with sizeof(struct mgmt_cp_add_advertising), 8 bytes more than the kernel's \
             exact length check accepts, so every LEAdvertisement1 registration fails on this \
             host, `bluetoothctl advertise on` included. Confirm with `journalctl -u bluetooth`: \
             `add_client_complete() Failed to add advertisement: Invalid Parameters (0x0d)`. \
             Not retried: each failed registration leaks a kernel advertising instance \
             (`btmgmt advinfo`; `sudo systemctl restart bluetooth` frees them). Workaround: \
             rerun with `--linux-advertising mgmt-legacy` (needs CAP_NET_ADMIN on the binary, \
             see the README Linux section)."
        ),
    }
}

/// The on-disk record of the advertising instance this sim added, so a run
/// that died without cleanup (SIGKILL, power loss of the process) is cleaned
/// up by the next start. Valid only within the boot that wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceRecord {
    pub boot_id: String,
    pub index: u16,
    pub instance: u8,
}

impl InstanceRecord {
    pub fn encode(&self) -> String {
        format!(
            "boot_id={}\nindex={}\ninstance={}\n",
            self.boot_id, self.index, self.instance
        )
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let field = |name: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(name)?.strip_prefix('='))
                .map(str::trim)
                .ok_or_else(|| format!("instance record has no {name}"))
        };
        Ok(Self {
            boot_id: field("boot_id")?.to_string(),
            index: field("index")?
                .parse()
                .map_err(|error| format!("instance record index: {error}"))?,
            instance: field("instance")?
                .parse()
                .map_err(|error| format!("instance record instance: {error}"))?,
        })
    }
}

/// What to do with a record found at start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleAction {
    /// Same boot, same controller, still listed by the kernel: ours, remove it.
    Remove(u8),
    /// Written by another boot or for another controller, or the instance is
    /// already gone: only the record is stale.
    DiscardRecord,
}

/// On-disk record of the adapter alias this sim replaced, so a run that died
/// without restoring it (SIGKILL, panic before the hook ran) is still
/// restored: the next start adopts `prev_alias` when the live alias is the
/// leftover sim name. Valid only within the boot that wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasRecord {
    pub boot_id: String,
    pub index: u16,
    pub prev_alias: String,
}

impl AliasRecord {
    pub fn encode(&self) -> String {
        format!(
            "boot_id={}\nindex={}\nprev_alias={}\n",
            self.boot_id, self.index, self.prev_alias
        )
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let field = |name: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(name)?.strip_prefix('='))
                .map(str::to_string)
                .ok_or_else(|| format!("alias record has no {name}"))
        };
        Ok(Self {
            boot_id: field("boot_id")?,
            index: field("index")?
                .parse()
                .map_err(|error| format!("alias record index: {error}"))?,
            prev_alias: field("prev_alias")?,
        })
    }
}

/// Path of the alias record for one controller: beside the MGMT instance
/// record, with its own name so the two lifecycles never share a file.
pub fn alias_record_path(index: u16) -> std::path::PathBuf {
    let directory = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    directory.join(format!("h10-sim-alias-hci{index}.instance"))
}

/// What claiming the adapter alias does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasPlan {
    /// The alias to set: always the advertised name.
    pub set_to: String,
    /// The alias to restore at stop (`None`: it already was the sim name,
    /// nothing to restore).
    pub restore_to: Option<String>,
    /// The restore target came from a stale record, not the live alias.
    pub adopted_stale: bool,
}

/// Plans the adapter-alias claim. `stale_prev_alias` is the previous alias a
/// same-boot record names, if any. A stale record only wins when the live
/// alias is the leftover sim name — if someone changed the alias after the
/// crash, the live alias is the truth.
pub fn plan_adapter_alias(
    current: &str,
    sim_name: &str,
    stale_prev_alias: Option<&str>,
) -> AliasPlan {
    if current == sim_name {
        if let Some(previous) = stale_prev_alias {
            return AliasPlan {
                set_to: sim_name.to_string(),
                restore_to: Some(previous.to_string()),
                adopted_stale: true,
            };
        }
        return AliasPlan {
            set_to: sim_name.to_string(),
            restore_to: None,
            adopted_stale: false,
        };
    }
    AliasPlan {
        set_to: sim_name.to_string(),
        restore_to: Some(current.to_string()),
        adopted_stale: false,
    }
}

/// The `bluetoothctl` invocation that restores a previous adapter alias from
/// a synchronous context (the panic hook): `system-alias <prev>`, or
/// `reset-alias` when the previous alias was empty (back to the system name).
pub fn system_alias_command(prev_alias: &str) -> (String, Vec<String>) {
    if prev_alias.is_empty() {
        ("bluetoothctl".to_string(), vec!["reset-alias".to_string()])
    } else {
        (
            "bluetoothctl".to_string(),
            vec!["system-alias".to_string(), prev_alias.to_string()],
        )
    }
}

/// Restores the recorded previous alias when the main thread panics after the
/// async radio is gone. The alias record carries everything the hook needs;
/// a failed restore keeps the record so the next start adopts it. A panic in
/// a side task leaves the process — and its alias — running, so it is left
/// alone here, like the MGMT instance hook.
pub fn install_adapter_alias_panic_hook(record: std::path::PathBuf) {
    use std::sync::Once;
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            previous(info);
            if std::thread::current().name() != Some("main") {
                return;
            }
            restore_alias_from_record(&record);
        }));
    });
}

/// Reads one alias record and restores its previous alias. Loud in every
/// outcome; returns whether the record is gone.
fn restore_alias_from_record(record: &std::path::Path) -> bool {
    let text = match std::fs::read_to_string(record) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return true;
        }
        Err(error) => {
            eprintln!(
                "h10-sim: panic cleanup could NOT read alias record {}: {error} \
                 (the adapter alias is left behind; fix it with \
                 `bluetoothctl system-alias <name>`)",
                record.display()
            );
            return false;
        }
    };
    let parsed = match AliasRecord::parse(&text) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!(
                "h10-sim: panic cleanup discards unreadable alias record {}: {error}",
                record.display()
            );
            remove_alias_record(record);
            return true;
        }
    };
    let (program, args) = system_alias_command(&parsed.prev_alias);
    match std::process::Command::new(&program).args(&args).output() {
        Ok(output) if output.status.success() => {
            eprintln!(
                "h10-sim: panic cleanup restored adapter alias on hci{} to {:?}",
                parsed.index, parsed.prev_alias
            );
            remove_alias_record(record);
            true
        }
        Ok(output) => {
            eprintln!(
                "h10-sim: panic cleanup could NOT restore adapter alias on hci{} to {:?}: \
                 {program} {} exited {}: {} (the next start adopts the record at {})",
                parsed.index,
                parsed.prev_alias,
                args.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim(),
                record.display()
            );
            false
        }
        Err(error) => {
            eprintln!(
                "h10-sim: panic cleanup could NOT run {program} to restore adapter alias on \
                 hci{} to {:?}: {error} (the next start adopts the record at {})",
                parsed.index,
                parsed.prev_alias,
                record.display()
            );
            false
        }
    }
}

/// The kernel boot id, scoping both record files to this boot: instances
/// and aliases never survive a reboot, so neither do the records.
pub fn boot_id() -> Result<String, String> {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|text| text.trim().to_string())
        .map_err(|error| format!("read /proc/sys/kernel/random/boot_id: {error}"))
}

pub(crate) fn remove_alias_record(path: &std::path::Path) {
    if let Err(error) = std::fs::remove_file(path) {
        if error.kind() != std::io::ErrorKind::NotFound {
            eprintln!(
                "h10-sim: cannot remove alias record {}: {error}",
                path.display()
            );
        }
    }
}

pub fn stale_action(
    record: &InstanceRecord,
    boot_id: &str,
    index: u16,
    listed: &[u8],
) -> StaleAction {
    if record.boot_id == boot_id && record.index == index && listed.contains(&record.instance) {
        StaleAction::Remove(record.instance)
    } else {
        StaleAction::DiscardRecord
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_parses_both_values_and_refuses_others() {
        assert_eq!(LinuxAdvertising::default(), LinuxAdvertising::Bluez);
        assert_eq!(
            LinuxAdvertising::parse("bluez"),
            Ok(LinuxAdvertising::Bluez)
        );
        assert_eq!(
            LinuxAdvertising::parse("mgmt-legacy"),
            Ok(LinuxAdvertising::MgmtLegacy)
        );
        assert_eq!(LinuxAdvertising::MgmtLegacy.as_str(), "mgmt-legacy");
        assert!(LinuxAdvertising::parse("mgmt").is_err());
    }

    #[test]
    fn cap_net_admin_is_read_from_capeff() {
        let root = "Name:\th10-sim\nCapInh:\t0000000000000000\nCapEff:\t000001ffffffffff\n";
        assert_eq!(effective_has_net_admin(root), Ok(true));
        let setcap = "CapPrm:\t0000000000001000\nCapEff:\t0000000000001000\n";
        assert_eq!(effective_has_net_admin(setcap), Ok(true));
        let user = "CapPrm:\t0000000000000000\nCapEff:\t0000000000000000\n";
        assert_eq!(effective_has_net_admin(user), Ok(false));
        // Permitted but not effective (setcap cap_net_admin+p without +e).
        let permitted_only = "CapPrm:\t0000000000001000\nCapEff:\t0000000000000000\n";
        assert_eq!(effective_has_net_admin(permitted_only), Ok(false));
        assert!(effective_has_net_admin("Name:\tx\n").is_err());
        assert!(effective_has_net_admin("CapEff:\tzz\n").is_err());
    }

    #[test]
    fn missing_capability_names_the_setcap_command() {
        let message = missing_capability_message("/home/u/h10-sim/target/debug/h10-sim");
        assert!(
            message.contains("`sudo setcap cap_net_admin+ep /home/u/h10-sim/target/debug/h10-sim`")
        );
        assert!(message.contains("`sudo setcap -r /home/u/h10-sim/target/debug/h10-sim`"));
        assert!(message.contains("never escalates"));
    }

    #[test]
    fn adapter_names_map_to_controller_indexes() {
        assert_eq!(hci_index("hci0"), Ok(0));
        assert_eq!(hci_index("hci12"), Ok(12));
        assert!(hci_index("usb0").is_err());
        assert!(hci_index("hci").is_err());
    }

    #[test]
    fn known_bluez_failure_names_the_mismatch_and_the_workaround() {
        let diagnosis = diagnose_bluez_registration(&BluezRegistrationFailure {
            failed_kind: true,
            message: "Failed to register advertisement",
            instances_before: "ActiveInstances=1 SupportedInstances=15",
            kernel_release: "7.0.0-30-generic",
        });
        assert!(diagnosis.known_mismatch);
        for needle in [
            "ActiveInstances=1 SupportedInstances=15",
            "kernel 7.0.0-30-generic",
            "MGMT_OP_ADD_EXT_ADV_DATA (0x0055)",
            "sizeof(struct mgmt_cp_add_advertising)",
            "Invalid Parameters (0x0d)",
            "Not retried",
            "--linux-advertising mgmt-legacy",
        ] {
            assert!(
                diagnosis.text.contains(needle),
                "{needle}: {}",
                diagnosis.text
            );
        }
    }

    #[test]
    fn other_bluez_failures_are_reported_as_they_are() {
        let diagnosis = diagnose_bluez_registration(&BluezRegistrationFailure {
            failed_kind: false,
            message: "Maximum advertisements reached",
            instances_before: "ActiveInstances=5 SupportedInstances=0",
            kernel_release: "6.8.0-139-generic",
        });
        assert!(!diagnosis.known_mismatch);
        assert!(diagnosis.text.contains("Maximum advertisements reached"));
        assert!(!diagnosis.text.contains("mgmt-legacy"));
    }

    #[test]
    fn instance_record_round_trips() {
        let record = InstanceRecord {
            boot_id: "8c1d2c61-0d7e-4c3f-9d0b-5a1e1f0e2a11".to_string(),
            index: 0,
            instance: 2,
        };
        assert_eq!(
            record.encode(),
            "boot_id=8c1d2c61-0d7e-4c3f-9d0b-5a1e1f0e2a11\nindex=0\ninstance=2\n"
        );
        assert_eq!(InstanceRecord::parse(&record.encode()), Ok(record));
        assert!(InstanceRecord::parse("boot_id=x\nindex=0\n").is_err());
        assert!(InstanceRecord::parse("boot_id=x\nindex=0\ninstance=300\n").is_err());
    }

    #[test]
    fn alias_record_round_trips() {
        let record = AliasRecord {
            boot_id: "boot-a".to_string(),
            index: 0,
            prev_alias: "lx5090".to_string(),
        };
        assert_eq!(
            record.encode(),
            "boot_id=boot-a\nindex=0\nprev_alias=lx5090\n"
        );
        assert_eq!(AliasRecord::parse(&record.encode()), Ok(record));
        assert!(AliasRecord::parse("boot_id=x\nindex=0\n").is_err());
        assert!(AliasRecord::parse("boot_id=x\nindex=0\nprev_alias=\n").is_ok());
    }

    #[test]
    fn alias_record_path_uses_the_runtime_dir() {
        let path = alias_record_path(0);
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("h10-sim-alias-hci0.instance")
        );
    }

    #[test]
    fn alias_plan_sets_the_sim_name_and_restores_the_previous() {
        // Normal start: alias differs, restore target is the live alias.
        let plan = plan_adapter_alias("lx5090", "Polar H10 SIM0001", None);
        assert_eq!(plan.set_to, "Polar H10 SIM0001");
        assert_eq!(plan.restore_to.as_deref(), Some("lx5090"));
        assert!(!plan.adopted_stale);
        // Alias already the sim name, no record: nothing to restore.
        let plan = plan_adapter_alias("Polar H10 SIM0001", "Polar H10 SIM0001", None);
        assert_eq!(plan.set_to, "Polar H10 SIM0001");
        assert_eq!(plan.restore_to, None);
        assert!(!plan.adopted_stale);
        // Leftover alias with a same-boot record: adopt the recorded
        // original, not the leftover sim name.
        let plan = plan_adapter_alias("Polar H10 SIM0001", "Polar H10 SIM0001", Some("lx5090"));
        assert_eq!(plan.set_to, "Polar H10 SIM0001");
        assert_eq!(plan.restore_to.as_deref(), Some("lx5090"));
        assert!(plan.adopted_stale);
        // Someone changed the alias after the crash: the live alias wins.
        let plan = plan_adapter_alias("other-host", "Polar H10 SIM0001", Some("lx5090"));
        assert_eq!(plan.restore_to.as_deref(), Some("other-host"));
        assert!(!plan.adopted_stale);
    }

    #[test]
    fn alias_restore_maps_to_bluetoothctl() {
        assert_eq!(
            system_alias_command("lx5090"),
            (
                "bluetoothctl".to_string(),
                vec!["system-alias".to_string(), "lx5090".to_string()]
            )
        );
        // An empty previous alias resets to the system name.
        assert_eq!(
            system_alias_command(""),
            ("bluetoothctl".to_string(), vec!["reset-alias".to_string()])
        );
    }

    #[test]
    fn stale_instance_of_this_boot_is_removed_otherwise_only_the_record_goes() {
        let record = InstanceRecord {
            boot_id: "boot-a".to_string(),
            index: 0,
            instance: 2,
        };
        assert_eq!(
            stale_action(&record, "boot-a", 0, &[1, 2]),
            StaleAction::Remove(2)
        );
        assert_eq!(
            stale_action(&record, "boot-b", 0, &[1, 2]),
            StaleAction::DiscardRecord,
            "instances never survive a reboot; number 2 now belongs to someone else"
        );
        assert_eq!(
            stale_action(&record, "boot-a", 1, &[1, 2]),
            StaleAction::DiscardRecord
        );
        assert_eq!(
            stale_action(&record, "boot-a", 0, &[1]),
            StaleAction::DiscardRecord,
            "already gone"
        );
    }
}
