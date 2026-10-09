mod acc;
mod acc_stream;
mod advertisement;
mod compare;
mod control;
#[cfg(any(test, target_os = "linux"))]
mod daemon_lifetime;
mod driver;
mod ecg;
mod events;
mod fidelity;
mod gatt_spec;
mod linux_advertising;
mod mgmt;
mod profile;
mod psftp;
mod radio;
mod sample_clock;
mod sim;
mod timing;
mod vectors;

use std::process::ExitCode;
use std::time::{Duration, Instant};

use chrono::Utc;
use radio::{
    adv_service_uuids, h10_services, short_of, PeripheralRadio, PlatformRadio, RadioEvent,
    SendOutcome,
};
use serde_json::json;
use sim::{PmdAction, SimConfig, SimState};
use tokio::sync::mpsc;
use uuid::Uuid;

use control::{apply_rates, ControlCommand, ControlReply, ControlRequest};
use events::EventLog;
use linux_advertising::LinuxAdvertising;

fn usage() -> String {
    ("h10-sim: Polar H10 BLE peripheral simulator (test tool)\n\
         \n\
         Usage: h10-sim [options]\n\
         \n\
         Options:\n\
         \x20 --profile <path>       JSON device profile (default: built-in stock-h10)\n\
         \x20 --name <name>          Advertised name, must start SIM (overrides profile)\n\
         \x20 --adapter <hciN>       Linux only: exact adapter; missing/refused never falls back\n\
         \x20 --control-bind <addr>  Control port bind address (default 127.0.0.1)\n\
         \x20 --control-port <port>  JSON-lines TCP control port (default 17935)\n\
         \x20 --control-token <tok>  Control port token (or H10SIM_TOKEN / --control-token-file)\n\
         \x20 --control-token-file <path>  Read the control token from a file\n\
         \x20 --pair-policy <policy> Pairing policy: just-works (default) or disabled\n\
         \x20 --bpm <bpm>            Heart rate in bpm (overrides the profile)\n\
         \x20 --battery <percent>    Battery level percent (overrides the profile)\n\
         \x20 --clock <mode>         ECG timestamp clock: polar-epoch (default, like the strap)\n\
         \x20                        or unsynchronized (boot-relative, no wall clock)\n\
         \x20 --mode <mode>          Run posture: faithful (default, reproduces the captured\n\
         \x20                        H10 behaviour, injects nothing) or adversarial (allows labelled\n\
         \x20                        fault injection through explicit control commands)\n\
         \x20 --drop-link-allow <addr>  Extra Bluetooth address drop-link disconnects on top of\n\
         \x20                        the tracked GATT clients (repeatable; only listed addresses\n\
         \x20                        and tracked clients are ever touched)\n\
         \x20 --ecg-file <path>      Replay recorded ECG (text, one integer µV per line @130 Hz)\n\
         \x20 --hr-replay <path>     Replay recorded HR packets (a raw capture JSON: raw.hrMeasurements)\n\
         \x20 --driver <url>        Join the test driver as a peripheral-sim host (ws://host:port/path)\n\
         \x20 --emit-driver-hello    Print the driver hello JSON and exit (no radio)\n\
         \x20 --linux-advertising <mode>  Linux only: bluez (default, LEAdvertisement1 via bluetoothd)\n\
         \x20                        or mgmt-legacy (MGMT_OP_ADD_ADVERTISING; needs CAP_NET_ADMIN, see README)\n\
         \n\
         \x20 A non-loopback --control-bind without a token refuses to start.\n\
         \x20 --emit-test-vectors    Print encoder vectors as JSON and exit (no radio)\n\
         \x20 --timing-profile <path>  Timing profile: an h10-capture fingerprint or timing JSON (default: measured strap profile)\n\
         \x20 --timing-seed <u64>    Seed for semi-random timing sampling (default 0; same seed replays a run)\n\
         \x20 --emit-timing-defaults  Print the UNCONFIRMED default timing profile as JSON and exit (no radio)\n\
         \x20 --emit-sim-fingerprint  Print the in-process sim fingerprint as JSON and exit (no radio)\n\
         \x20 --compare <real.json> <sim.json>  Compare fingerprints field by field, print the report and exit (no radio); exit 0 needs passed AND complete\n\
         \x20 --allow-incomplete      With --compare, exit 0 on passed even when coverage is incomplete\n\
         \x20 --qualify-ota <real.json> <sim-run.json>  OTA qualification: real strap capture vs real simulator run (no radio); exit 0 needs passed AND complete (--allow-incomplete never applies)\n\
         \x20 --tolerance-p50 <f>    Relative p50 tolerance for timing checks (default 0.25)\n\
         \x20 --tolerance-ms <f>     Absolute floor in ms for timing checks (default 50)\n\
         \x20 --help                Print this help\n\
         \n\
         \x20 Later flags win: --profile applies first, then --name/--bpm/--battery.\n")
        .to_string()
}

/// Stock identity, compiled in so the default works from any directory.
const STOCK_PROFILE_JSON: &str = include_str!("../profiles/stock-h10.json");

/// Measured timing, compiled in so the default works from any directory:
/// `profiles/timing-h10-measured.json`, fitted from the Tauri strap capture.
const MEASURED_TIMING_JSON: &str = include_str!("../profiles/timing-h10-measured.json");

/// Reads a timing profile: a full `h10-capture` fingerprint or a raw timing
/// profile document. A bad path or bad file is a loud startup failure —
/// never a silent fall back to defaults.
fn load_timing_profile(path: &str, seed: u64) -> Result<timing::TimingProfile, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read timing profile {path}: {error}"))?;
    timing::TimingProfile::from_fingerprint_json(&text, seed)
}

/// Loads two `h10-capture` fingerprints and compares them. A bad path or
/// bad file is a loud failure — never an assumption.
fn load_comparison(
    real_path: &str,
    sim_path: &str,
    tolerances: compare::Tolerances,
) -> Result<compare::ComparisonReport, String> {
    let read = |path: &str| {
        std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read {path}: {error}"))
            .and_then(|text| {
                serde_json::from_str(&text).map_err(|error| format!("cannot parse {path}: {error}"))
            })
    };
    let real: serde_json::Value = read(real_path)?;
    let sim: serde_json::Value = read(sim_path)?;
    Ok(compare::compare_fingerprints(&real, &sim, tolerances))
}

/// Prints the field-by-field report as JSON. An encoding failure is a loud
/// failure, never a missing report.
fn print_comparison_report(report: &compare::ComparisonReport) -> Result<(), String> {
    match serde_json::to_string_pretty(report) {
        Ok(json) => {
            println!("{json}");
            Ok(())
        }
        Err(error) => Err(format!("cannot encode comparison report: {error}")),
    }
}

/// Names the unverified fields on stderr. Incomplete corners are reported
/// either way, so the gap is never silent.
fn report_incomplete_fields(report: &compare::ComparisonReport) {
    if !report.complete {
        let incomplete: Vec<&str> = report
            .fields
            .iter()
            .filter(|field| field.status == compare::CheckStatus::Incomplete)
            .map(|field| field.field.as_str())
            .collect();
        eprintln!(
            "h10-sim: comparison incomplete, unverified fields: {}",
            incomplete.join(", ")
        );
    }
}

/// Compares a real-strap fingerprint against a simulator fingerprint and
/// prints the field-by-field report as JSON. Exits 0 only when every check
/// passed AND coverage is complete; 1 otherwise. A run with `passed` but
/// `complete:false` (unverified corners: rotated payload bytes, rows without
/// parent paths, thin samples) must never read as qualified — pass
/// `--allow-incomplete` to accept that explicitly. Incomplete field names go
/// to stderr either way, so the gap is never silent.
fn run_compare(
    real_path: &str,
    sim_path: &str,
    tolerances: compare::Tolerances,
    allow_incomplete: bool,
) -> ExitCode {
    match load_comparison(real_path, sim_path, tolerances) {
        Ok(report) => {
            if let Err(message) = print_comparison_report(&report) {
                eprintln!("h10-sim: {message}");
                return ExitCode::FAILURE;
            }
            report_incomplete_fields(&report);
            compare_exit_code(&report, allow_incomplete)
        }
        Err(message) => {
            eprintln!("h10-sim: {message}");
            ExitCode::from(2)
        }
    }
}

/// Over-the-air qualification: a real strap capture compared against a real
/// simulator run (both produced by the same `h10-capture` scenario over the
/// air). Exits 0 only when every check passed AND coverage is complete;
/// 1 otherwise, with the failed or unverified field names on stderr.
/// `--allow-incomplete` never applies here — qualification is `passed &&
/// complete`, full stop. The in-process `--emit-sim-fingerprint` document
/// is structural evidence only and cannot qualify. Both capture paths are
/// required: the argument parser rejects a missing path loudly (exit 2),
/// so this entry point never runs without captures and never silently
/// skips.
fn run_qualify_ota(
    real_path: &str,
    sim_run_path: &str,
    tolerances: compare::Tolerances,
) -> ExitCode {
    match load_comparison(real_path, sim_run_path, tolerances) {
        Ok(report) => {
            if let Err(message) = print_comparison_report(&report) {
                eprintln!("h10-sim: {message}");
                return ExitCode::FAILURE;
            }
            report_incomplete_fields(&report);
            match compare::qualify_ota(&report) {
                Ok(()) => ExitCode::SUCCESS,
                Err(refusal) => {
                    eprintln!("h10-sim: {refusal}");
                    ExitCode::from(1)
                }
            }
        }
        Err(message) => {
            eprintln!("h10-sim: {message}");
            ExitCode::from(2)
        }
    }
}

/// Exit code for a comparison report: 0 only when every check passed AND
/// coverage is complete, unless `--allow-incomplete` explicitly accepts a
/// passed-but-incomplete run. Pure so the qualification gate is unit-pinned.
fn compare_exit_code(report: &compare::ComparisonReport, allow_incomplete: bool) -> ExitCode {
    if report.passed && (report.complete || allow_incomplete) {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn main() -> ExitCode {
    let mut config = SimConfig::default();
    // The stock profile is the default identity; explicit flags override it.
    match profile::parse_profile(STOCK_PROFILE_JSON, "<builtin stock-h10>") {
        Ok(stock) => {
            if let Err(message) = config.apply_profile("<builtin stock-h10>", &stock) {
                eprintln!("h10-sim: {message}");
                return ExitCode::from(2);
            }
        }
        Err(message) => {
            eprintln!("h10-sim: {message}");
            return ExitCode::from(2);
        }
    }
    let mut control_bind = "127.0.0.1".to_string();
    let mut control_port: u16 = 17935;
    let mut control_token: Option<String> = None;
    let mut control_token_file: Option<String> = None;
    let mut driver_url: Option<String> = None;
    let mut emit_vectors = false;
    let mut emit_driver_hello = false;
    let mut emit_timing_defaults = false;
    let mut emit_sim_fingerprint = false;
    let mut timing_profile_path: Option<String> = None;
    let mut timing_seed: u64 = 0;
    let mut run_mode = control::RunMode::default();
    let mut compare_paths: Option<(String, String)> = None;
    let mut qualify_paths: Option<(String, String)> = None;
    let mut allow_incomplete = false;
    let mut tolerance_p50 = compare::Tolerances::default().p50_relative;
    let mut tolerance_ms = compare::Tolerances::default().min_abs_ms;
    let mut linux_advertising = LinuxAdvertising::default();
    let mut adapter_name: Option<String> = None;

    let mut args = std::env::args().skip(1).peekable();
    while let Some(arg) = args.next() {
        let value = |args: &mut std::iter::Peekable<std::iter::Skip<std::env::Args>>,
                     flag: &str|
         -> Result<String, String> {
            args.next()
                .ok_or_else(|| format!("{flag} requires a value"))
        };
        let parsed: Result<(), String> = (|| {
            match arg.as_str() {
                "--profile" => {
                    let path = value(&mut args, "--profile")?;
                    load_startup_profile(&mut config, &path)?;
                }
                "--name" => {
                    let name = value(&mut args, "--name")?;
                    advertisement::validate_simulator_name(&name)?;
                    config.name = name;
                }
                "--adapter" => {
                    let name = value(&mut args, "--adapter")?;
                    linux_advertising::hci_index(&name)?;
                    if !cfg!(target_os = "linux") {
                        return Err("--adapter is supported only on Linux".to_string());
                    }
                    adapter_name = Some(name);
                }
                "--control-bind" => control_bind = value(&mut args, "--control-bind")?,
                "--control-port" => {
                    control_port = value(&mut args, "--control-port")?
                        .parse::<u16>()
                        .map_err(|_| "--control-port must be 0-65535".to_string())?;
                }
                "--control-token" => control_token = Some(value(&mut args, "--control-token")?),
                "--control-token-file" => {
                    control_token_file = Some(value(&mut args, "--control-token-file")?);
                }
                "--pair-policy" => {
                    config.pair_policy =
                        sim::PairPolicy::parse(&value(&mut args, "--pair-policy")?)?;
                }
                "--bpm" => {
                    config.bpm = value(&mut args, "--bpm")?
                        .parse::<u8>()
                        .map_err(|_| "--bpm must be 0-255".to_string())?;
                }
                "--battery" => {
                    config.battery_percent = value(&mut args, "--battery")?
                        .parse::<u8>()
                        .map_err(|_| "--battery must be 0-255".to_string())?;
                    if config.battery_percent > 100 {
                        return Err("--battery must be 0-100".to_string());
                    }
                }
                "--clock" => {
                    config.clock = sim::DeviceClock::parse(&value(&mut args, "--clock")?)?;
                }
                "--mode" => {
                    run_mode = control::RunMode::parse(&value(&mut args, "--mode")?)?;
                }
                "--drop-link-allow" => {
                    let address = value(&mut args, "--drop-link-allow")?;
                    radio::parse_bt_address(&address)
                        .map_err(|error| format!("--drop-link-allow {address:?}: {error}"))?;
                    config.drop_link_allowlist.push(address);
                }
                "--ecg-file" => {
                    let file = value(&mut args, "--ecg-file")?;
                    config.ecg_source = sim::EcgSource::File { file };
                }
                "--hr-replay" => {
                    let file = value(&mut args, "--hr-replay")?;
                    config.hr_source = sim::HrSource::File { file };
                }
                "--emit-test-vectors" => emit_vectors = true,
                "--driver" => driver_url = Some(value(&mut args, "--driver")?),
                "--emit-driver-hello" => emit_driver_hello = true,
                "--linux-advertising" => {
                    linux_advertising =
                        LinuxAdvertising::parse(&value(&mut args, "--linux-advertising")?)?;
                    if !cfg!(target_os = "linux") {
                        return Err(format!(
                            "--linux-advertising applies to Linux only (this is {})",
                            std::env::consts::OS
                        ));
                    }
                }
                "--emit-timing-defaults" => emit_timing_defaults = true,
                "--emit-sim-fingerprint" => emit_sim_fingerprint = true,
                "--timing-profile" => {
                    timing_profile_path = Some(value(&mut args, "--timing-profile")?)
                }
                "--timing-seed" => {
                    timing_seed = value(&mut args, "--timing-seed")?
                        .parse::<u64>()
                        .map_err(|_| "--timing-seed must be 0-18446744073709551615".to_string())?;
                }
                "--compare" => {
                    let real = value(&mut args, "--compare")?;
                    let sim = value(&mut args, "--compare")?;
                    compare_paths = Some((real, sim));
                }
                "--qualify-ota" => {
                    let real = value(&mut args, "--qualify-ota")?;
                    let sim_run = value(&mut args, "--qualify-ota")?;
                    qualify_paths = Some((real, sim_run));
                }
                "--allow-incomplete" => allow_incomplete = true,
                "--tolerance-p50" => {
                    tolerance_p50 = value(&mut args, "--tolerance-p50")?
                        .parse::<f64>()
                        .map_err(|_| "--tolerance-p50 must be a number >= 0".to_string())?;
                    if tolerance_p50.is_sign_negative() {
                        return Err("--tolerance-p50 must be a number >= 0".to_string());
                    }
                }
                "--tolerance-ms" => {
                    tolerance_ms = value(&mut args, "--tolerance-ms")?
                        .parse::<f64>()
                        .map_err(|_| "--tolerance-ms must be a number >= 0".to_string())?;
                    if tolerance_ms.is_sign_negative() {
                        return Err("--tolerance-ms must be a number >= 0".to_string());
                    }
                }
                "--help" => {
                    print!("{}", usage());
                    std::process::exit(0);
                }
                other => return Err(format!("unknown argument {other}; see --help")),
            }
            Ok(())
        })();
        if let Err(message) = parsed {
            eprintln!("h10-sim: {message}");
            return ExitCode::from(2);
        }
    }

    // Token precedence: flag, then file, then environment. An unreadable
    // token file is a loud startup failure; the token value is never logged.
    if control_token.is_none() {
        if let Some(path) = control_token_file {
            let raw = std::fs::read_to_string(&path)
                .map_err(|error| format!("cannot read control token file {path}: {error}"));
            match raw {
                Ok(text) => {
                    let trimmed = text.trim().to_string();
                    if !trimmed.is_empty() {
                        control_token = Some(trimmed);
                    }
                }
                Err(message) => {
                    eprintln!("h10-sim: {message}");
                    return ExitCode::from(2);
                }
            }
        }
    }
    if control_token.is_none() {
        if let Ok(from_env) = std::env::var("H10SIM_TOKEN") {
            if !from_env.trim().is_empty() {
                control_token = Some(from_env.trim().to_string());
            }
        }
    }
    if let Err(message) = control::check_bind(&control_bind, control_token.as_deref()) {
        eprintln!("h10-sim: {message}");
        return ExitCode::from(2);
    }

    if emit_driver_hello {
        println!("{}", driver::hello_json("h10-sim"));
        return ExitCode::SUCCESS;
    }
    if emit_timing_defaults {
        match serde_json::to_string_pretty(&timing::TimingProfile::default_unconfirmed(timing_seed))
        {
            Ok(json) => {
                println!("{json}");
                return ExitCode::SUCCESS;
            }
            Err(error) => {
                eprintln!("h10-sim: cannot encode timing defaults: {error}");
                return ExitCode::FAILURE;
            }
        }
    }
    if let Some((real, sim_run)) = qualify_paths {
        return run_qualify_ota(
            &real,
            &sim_run,
            compare::Tolerances {
                p50_relative: tolerance_p50,
                min_abs_ms: tolerance_ms,
            },
        );
    }
    if let Some((real, sim)) = compare_paths {
        return run_compare(
            &real,
            &sim,
            compare::Tolerances {
                p50_relative: tolerance_p50,
                min_abs_ms: tolerance_ms,
            },
            allow_incomplete,
        );
    }
    // Timing runs on the measured strap profile by default; an explicit
    // --timing-profile overrides it, and a bad profile path is a loud
    // startup failure.
    let timing_profile = match timing_profile_path {
        Some(path) => match load_timing_profile(&path, timing_seed) {
            Ok(profile) => profile,
            Err(message) => {
                eprintln!("h10-sim: {message}");
                return ExitCode::from(2);
            }
        },
        None => {
            match timing::TimingProfile::from_fingerprint_json(MEASURED_TIMING_JSON, timing_seed) {
                Ok(profile) => profile,
                Err(message) => {
                    eprintln!("h10-sim: built-in measured timing profile is corrupt: {message}");
                    return ExitCode::from(2);
                }
            }
        }
    };
    if emit_sim_fingerprint {
        // The same builder the fidelity test compares: stock identity with
        // CLI overrides applied, measured timing unless overridden.
        match fidelity::sim_fingerprint(&config, &timing_profile) {
            Ok(fingerprint) => match serde_json::to_string_pretty(&fingerprint) {
                Ok(json) => {
                    println!("{json}");
                    return ExitCode::SUCCESS;
                }
                Err(error) => {
                    eprintln!("h10-sim: cannot encode sim fingerprint: {error}");
                    return ExitCode::FAILURE;
                }
            },
            Err(message) => {
                eprintln!("h10-sim: {message}");
                return ExitCode::from(2);
            }
        }
    }
    if emit_vectors {
        let state = SimState::new(config);
        match serde_json::to_string(&vectors::test_vectors(&state)) {
            Ok(json) => {
                println!("{json}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("h10-sim: cannot encode test vectors: {error}");
                ExitCode::FAILURE
            }
        }
    } else {
        run(
            config,
            control_bind,
            control_port,
            control_token,
            driver_url,
            timing_profile,
            linux_advertising,
            adapter_name,
            run_mode,
        )
    }
}

/// Loads a profile file onto the startup config. A bad path or bad file is a
/// loud startup failure — never a silent fall back to defaults.
fn load_startup_profile(config: &mut SimConfig, path: &str) -> Result<(), String> {
    let profile = profile::load_profile(path)?;
    config.apply_profile(path, &profile)
}

/// Startup plumbing: every run input arrives here explicitly rather than
/// through globals, so the argument count stays honest about it.
#[allow(clippy::too_many_arguments)]
fn run(
    config: SimConfig,
    control_bind: String,
    control_port: u16,
    control_token: Option<String>,
    driver_url: Option<String>,
    timing_profile: timing::TimingProfile,
    linux_advertising: LinuxAdvertising,
    adapter_name: Option<String>,
    run_mode: control::RunMode,
) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("h10-sim: cannot start async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(serve(
        config,
        control_bind,
        control_port,
        control_token,
        driver_url,
        timing_profile,
        linux_advertising,
        adapter_name,
        run_mode,
    )) {
        Ok(()) => ExitCode::SUCCESS,
        Err(fatal) => {
            eprintln!("h10-sim: {}", fatal.message);
            ExitCode::from(fatal.exit)
        }
    }
}

/// A startup or runtime failure that ends the process, with its exit status:
/// 1 by default, [`linux_advertising::EXIT_ADVERTISING_UNAVAILABLE`] when a
/// restart cannot help.
struct Fatal {
    message: String,
    exit: u8,
}

impl From<String> for Fatal {
    fn from(message: String) -> Self {
        Self { message, exit: 1 }
    }
}

impl Fatal {
    fn unavailable(message: String) -> Self {
        Self {
            message,
            exit: linux_advertising::EXIT_ADVERTISING_UNAVAILABLE,
        }
    }
}

/// Resolves when the process is asked to stop: SIGINT everywhere, SIGTERM
/// (systemd stop, `kill`) on Unix. Returns the signal name for the log.
async fn shutdown_signal() -> Result<&'static str, String> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut terminate = signal(SignalKind::terminate())
            .map_err(|error| format!("cannot watch SIGTERM: {error}"))?;
        tokio::select! {
            interrupted = tokio::signal::ctrl_c() => interrupted
                .map(|()| "SIGINT")
                .map_err(|error| format!("cannot watch SIGINT: {error}")),
            _ = terminate.recv() => Ok("SIGTERM"),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .map(|()| "ctrl-c")
            .map_err(|error| format!("cannot watch ctrl-c: {error}"))
    }
}

/// Selects the Linux advertising path before anything is registered.
/// `mgmt-legacy` without `CAP_NET_ADMIN` is refused with the exit status a
/// supervisor does not restart on.
#[cfg(target_os = "linux")]
fn select_linux_advertising(
    radio: &mut PlatformRadio,
    mode: LinuxAdvertising,
    log: &mut EventLog,
) -> Result<(), Fatal> {
    if mode == LinuxAdvertising::MgmtLegacy {
        radio::require_net_admin().map_err(Fatal::unavailable)?;
        let found = radio.use_mgmt_legacy().map_err(|error| error.to_string())?;
        log.log(
            "advertising-backend",
            json!({"mode": mode.as_str(), "mgmt": found}),
        );
    } else {
        log.log("advertising-backend", json!({"mode": mode.as_str()}));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn select_linux_advertising(
    _radio: &mut PlatformRadio,
    _mode: LinuxAdvertising,
    _log: &mut EventLog,
) -> Result<(), Fatal> {
    Ok(())
}

/// Resolves the configured ECG source to replay samples (None = synthetic).
/// A missing or malformed file — or a corrupt built-in recording — is a
/// loud startup/profile-load failure.
fn load_ecg_replay(source: &sim::EcgSource) -> Result<Option<Vec<i32>>, String> {
    match source {
        sim::EcgSource::Synthetic => Ok(None),
        sim::EcgSource::Recorded => ecg::recorded_samples().map(Some),
        sim::EcgSource::File { file } => ecg::load_replay_file(file).map(Some),
    }
}

/// Resolves the configured HR source to replay packets (None = synthetic).
/// A missing or malformed file is a loud startup/profile-load failure.
fn load_hr_replay(source: &sim::HrSource) -> Result<Option<sim::HrReplay>, String> {
    match source {
        sim::HrSource::Synthetic => Ok(None),
        sim::HrSource::File { file } => sim::load_hr_replay(file).map(Some),
    }
}

/// Startup plumbing, like [`run`]: explicit run inputs, not globals.
#[allow(clippy::too_many_arguments)]
async fn serve(
    config: SimConfig,
    control_bind: String,
    control_port: u16,
    control_token: Option<String>,
    driver_url: Option<String>,
    timing_profile: timing::TimingProfile,
    linux_advertising: LinuxAdvertising,
    adapter_name: Option<String>,
    run_mode: control::RunMode,
) -> Result<(), Fatal> {
    let mut log = EventLog::new();
    let mut sim = SimState::new(config);
    // The run posture is a startup decision, never a profile field: a
    // profile load cannot smuggle a faithful run into adversarial mode.
    sim.run_mode = run_mode;
    sim.run_seed = timing_profile.seed;
    log.log(
        "run-mode",
        json!({"mode": run_mode.as_str(), "seed": timing_profile.seed}),
    );
    sim.ecg_replay = load_ecg_replay(&sim.config.ecg_source)?;
    sim.hr_replay = load_hr_replay(&sim.config.hr_source)?;
    let mut timing = timing::TimingRuntime::new(timing_profile);
    log.log(
        "timing-profile",
        json!({"seed": timing.profile.seed, "confirmed": timing.profile.fully_confirmed(), "unconfirmed": timing.profile.unconfirmed_fields()}),
    );
    let boot_epoch_ns: u64 = Utc::now().timestamp_nanos_opt().unwrap_or(0).max(0) as u64;
    let sample_clock_boot = Instant::now();

    let (radio_tx, mut radio_rx) = mpsc::channel::<RadioEvent>(256);
    let mut radio = PlatformRadio::open_with_adapter(radio_tx, adapter_name.as_deref())
        .await
        .map_err(|error| error.to_string())?;

    wait_powered(&mut radio, &mut log).await?;
    select_linux_advertising(&mut radio, linux_advertising, &mut log)?;

    let services = h10_services(&sim.config).map_err(|error| error.to_string())?;
    let registered_service_uuids: Vec<String> = services
        .iter()
        .map(|service| service.uuid.to_string())
        .collect();
    for service in services {
        radio
            .add_service(&service)
            .await
            .map_err(|error| error.to_string())?;
    }
    log.log(
        "services-registered",
        json!({"services": registered_service_uuids}),
    );

    if let Err(message) = start_advertising(&mut radio, &sim, &mut log).await {
        if radio.fatal_failure().is_some() {
            stop_failed_radio(&mut radio, &mut sim, &mut log, &message).await;
        }
        return Err(if radio.advertising_unavailable() {
            Fatal::unavailable(message)
        } else {
            Fatal::from(message)
        });
    }

    let (control_tx, mut control_rx) = mpsc::channel::<ControlRequest>(64);
    // Bind before declaring readiness: `control-listening` is logged only
    // after the socket is bound, never on a spawn that may still fail.
    let control_listener = control::listen(&control_bind, control_port, control_token.as_deref())
        .await
        .map_err(Fatal::from)?;
    log.log(
        "control-listening",
        json!({"bind": control_bind, "port": control_port, "auth": control_token.is_some()}),
    );
    let control_server = tokio::spawn(control::serve_listener(
        control_listener,
        control_token.clone(),
        control_tx.clone(),
    ));

    if let Some(url) = driver_url {
        let (event_tx, event_rx) = mpsc::channel::<serde_json::Value>(256);
        log.add_listener(event_tx);
        log.log("driver-joining", json!({"url": url}));
        let driver_commands = control_tx.clone();
        let host_label = format!("peripheral-sim/{}", std::env::consts::OS);
        // `driver::run` rejoins forever and only returns when the simulator
        // loop itself is gone. Control state (mode, faults, profile, run
        // record) lives in the sim loop behind `driver_commands`, so it
        // survives every rejoin unreset. Without `--driver` this task simply
        // never spawns and the sim runs standalone.
        tokio::spawn(async move {
            match driver::run(url, host_label, driver_commands, event_rx).await {
                Ok(()) => eprintln!("h10-sim: driver task ended"),
                Err(error) => eprintln!("h10-sim: driver task ended: {error}"),
            }
        });
    }

    let mut next_hr = Instant::now();
    let mut next_ecg = Instant::now();
    let mut ecg_runtime = None;
    let mut acc_runtime = None;
    let mut psftp_session = psftp::Session::new();
    let mut next_battery = Instant::now() + Duration::from_secs(60);
    let mut last_tick = Instant::now();
    let boot = Instant::now();
    // The minimum ATT capacity fits one ACC sample; 200 Hz then needs a
    // five-millisecond opportunity, while actual batching follows capacity.
    let tick = tokio::time::interval(Duration::from_millis(5));
    tokio::pin!(tick);
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            signal = &mut shutdown => {
                let signal = signal?;
                log.log("shutdown", json!({"signal": signal}));
                control_server.abort();
                let detail = radio.advertising_detail();
                return match radio.stop_advertising().await {
                    Ok(()) => {
                        log.log("advertising-stopped", json!({"advertising": detail}));
                        Ok(())
                    }
                    Err(error) => {
                        log.log(
                            "radio-error",
                            json!({"op": "stop-advertising", "error": error.to_string()}),
                        );
                        Err(Fatal::from(format!("shutdown cleanup failed: {error}")))
                    }
                };
            }
            _ = tick.tick() => {
                if let Some(reason) = radio.fatal_failure() {
                    control_server.abort();
                    stop_failed_radio(&mut radio, &mut sim, &mut log, &reason).await;
                    return Err(Fatal::from(reason));
                }
                let now = Instant::now();
                let elapsed_s = now.duration_since(last_tick).as_secs_f64();
                last_tick = now;
                if sim.tick_battery(elapsed_s) {
                    send_battery(&mut radio, &sim, &mut log).await;
                }
                if !sim.config.bpm_curve.is_empty() {
                    sim.config.bpm = sim.curve_bpm(boot.elapsed().as_secs_f64());
                }
                if now >= next_hr {
                    next_hr = now + Duration::from_secs_f64(timing.hr_interval_s(sim.config.hr_hz));
                    send_hr(&mut radio, &mut sim, &mut log).await;
                }
                if now >= next_ecg {
                    let interval = Duration::from_secs_f64(timing.ecg_interval_s(sim.config.ecg_frames_per_sec));
                    // Skip missed dispatch slots, not sensor time. Acquisition
                    // accounting below reports any samples that cannot catch up.
                    next_ecg = now + sample_clock::next_dispatch_delay(now.duration_since(next_ecg), interval);
                    send_ecg(&mut radio, &mut sim, &mut log, &mut ecg_runtime, sample_clock_boot, boot_epoch_ns).await;
                }
                send_acc(&mut radio, &mut sim, &mut log, &mut acc_runtime, sample_clock_boot, boot_epoch_ns).await;
                if now >= next_battery {
                    next_battery = now + Duration::from_secs(60);
                    send_battery(&mut radio, &sim, &mut log).await;
                }
                drain_indications(&mut radio, &mut sim, &mut log).await;
            }
            Some(event) = radio_rx.recv() => {
                handle_radio(event, &mut radio, &mut sim, &mut timing, &mut log, &mut psftp_session, boot_epoch_ns).await;
            }
            Some(request) = control_rx.recv() => {
                handle_control(request, &mut radio, &mut sim, &mut log).await;
            }
        }
    }
}

async fn stop_failed_radio(
    radio: &mut PlatformRadio,
    sim: &mut SimState,
    log: &mut EventLog,
    reason: &str,
) {
    let loss = radio.retire_failed_collection();
    sim.reset_pmd_session();
    log.log(
        "radio-fatal",
        json!({"reason": reason, "collectionStopped": true, "delivery": loss}),
    );
    match tokio::time::timeout(Duration::from_secs(5), radio.stop_advertising()).await {
        Ok(Ok(())) => log.log("advertising-stopped", json!({"reason": "radio-fatal"})),
        Ok(Err(error)) => log.log("radio-error", json!({"op": "fatal-stop-advertising", "error": error.to_string(), "ownedInstanceCleanup": "unconfirmed"})),
        Err(_) => log.log("radio-error", json!({"op": "fatal-stop-advertising", "error": "cleanup deadline expired", "ownedInstanceCleanup": "unconfirmed"})),
    }
}

async fn wait_powered(radio: &mut PlatformRadio, log: &mut EventLog) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match radio.is_powered().await {
            Ok(true) => {
                log.log_simple("powered-on");
                return Ok(());
            }
            Ok(false) => {}
            Err(error) => {
                log.log(
                    "radio-error",
                    json!({"op": "is-powered", "error": error.to_string()}),
                );
            }
        }
        if Instant::now() >= deadline {
            return Err("Bluetooth adapter is not powered (waited 30 s)".to_string());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn start_advertising(
    radio: &mut impl PeripheralRadio,
    sim: &SimState,
    log: &mut EventLog,
) -> Result<(), String> {
    advertisement::validate_simulator_name(&sim.config.name)?;
    match radio.is_advertising().await {
        Ok(true) => {
            radio
                .stop_advertising()
                .await
                .map_err(|error| format!("replace the running advertisement: {error}"))?;
        }
        Ok(false) => {}
        Err(error) => {
            log.log(
                "radio-error",
                json!({"op": "is-advertising", "error": error.to_string()}),
            );
            return Err(error.to_string());
        }
    }
    // Manufacturer data spends advertisement-data budget: refuse loudly when
    // the profile configures more than fits.
    advertisement::check_mfr_budget(sim.config.mfr_payload.len())?;
    // The scan response holds the complete name (see `advertisement`): fit
    // the configured name to the 31-byte budget first, loudly when it changes.
    let fitted = advertisement::fit_name(&sim.config.name);
    if !advertisement::fits_budget(&sim.config.name) {
        log.log(
            "advertising-name-truncated",
            json!({"configured": sim.config.name, "effective": fitted}),
        );
    }
    let sizes = advertisement::advertisement_sizes(&fitted);
    radio
        .set_adv_manufacturer_data(sim.config.mfr_company, sim.config.mfr_payload.clone())
        .await
        .map_err(|error| error.to_string())?;
    radio
        .start_advertising(&fitted, &adv_service_uuids())
        .await
        .map_err(|error| error.to_string())?;
    // Whether the staged manufacturer data goes over the air depends on the
    // backend: BlueZ emits it, Apple has no such peripheral API. The log
    // always says which happened — never a silent omission.
    let manufacturer_data = if sim.config.mfr_payload.is_empty() {
        "none-configured"
    } else if radio.supports_manufacturer_data() {
        "emitted"
    } else {
        "omitted-platform-unsupported"
    };
    log.log(
        "advertising-started",
        json!({"name": fitted, "services": ["180D", "FEEE"], "advLen": sizes.adv_len, "scanRspLen": sizes.scan_rsp_len, "polarCompanyId": gatt_spec::POLAR_COMPANY_ID, "manufacturerData": manufacturer_data, "advertising": radio.advertising_detail()}),
    );
    Ok(())
}

async fn send_hr(radio: &mut PlatformRadio, sim: &mut SimState, log: &mut EventLog) {
    if sim.silent {
        return;
    }
    let payload = sim.hr_payload();
    let uuid = advertisement::short_uuid(gatt_spec::uuid16::HEART_RATE_MEASUREMENT);
    let outcome = radio.notify(uuid, payload.clone()).await;
    sim.observe_hr_notify(&outcome);
    report_stream_notify(log, "hr-notify", outcome, json!({"bpm": sim.config.bpm}));
}

/// Reports one inline notify outcome for a stream tick. `OsAccepted` and
/// `Queued` log the frame line (a queued frame settles later with its own
/// `notify-settled` line; the queued line is the request, the settled line
/// the answer). `NotSubscribed` is the normal no-central case — no line,
/// never an error. `Failed` is always loud.
fn report_stream_notify(
    log: &mut EventLog,
    op: &str,
    outcome: Result<SendOutcome, crate::radio::RadioError>,
    detail: serde_json::Value,
) {
    if let Some((kind, detail)) = stream_notify_log(op, outcome, detail) {
        log.log(kind, detail);
    }
}

/// Keep the frame identity beside its outcome, including rejected sends.
fn stream_notify_log(
    op: &str,
    outcome: Result<SendOutcome, crate::radio::RadioError>,
    detail: serde_json::Value,
) -> Option<(&str, serde_json::Value)> {
    let mut detail = match detail {
        serde_json::Value::Object(fields) => fields,
        other => serde_json::Map::from_iter([("detail".to_owned(), other)]),
    };
    let outcome = outcome.unwrap_or_else(|error| SendOutcome::Failed(error.to_string()));
    let kind = match outcome {
        SendOutcome::NotSubscribed => return None,
        SendOutcome::OsAccepted => {
            detail.insert("outcome".into(), json!("os-accepted"));
            op
        }
        SendOutcome::Queued { id } => {
            detail.insert("id".into(), json!(id));
            detail.insert("outcome".into(), json!("queued"));
            op
        }
        SendOutcome::Failed(reason) => {
            detail.insert("op".into(), json!(op));
            detail.insert("error".into(), json!(reason));
            "radio-error"
        }
    };
    Some((kind, serde_json::Value::Object(detail)))
}

async fn send_battery(radio: &mut PlatformRadio, sim: &SimState, log: &mut EventLog) {
    if sim.silent {
        return;
    }
    let payload = gatt_spec::encode_battery_level(sim.config.battery_percent);
    let uuid = advertisement::short_uuid(gatt_spec::uuid16::BATTERY_LEVEL);
    let outcome = radio.notify(uuid, payload).await;
    report_stream_notify(
        log,
        "battery-notify",
        outcome,
        json!({"percent": sim.config.battery_percent}),
    );
}

async fn send_ecg(
    radio: &mut impl PeripheralRadio,
    sim: &mut SimState,
    log: &mut EventLog,
    runtime: &mut Option<(Instant, sample_clock::SampleClock)>,
    clock_boot: Instant,
    boot_epoch_ns: u64,
) {
    let elapsed = sim
        .ecg_started_at
        .map(|started| started.elapsed())
        .unwrap_or_default();
    send_ecg_at(radio, sim, log, runtime, clock_boot, boot_epoch_ns, elapsed).await;
}

/// Snapshot acquisition time once per dispatch; packet count is not sensor time.
async fn send_ecg_at(
    radio: &mut impl PeripheralRadio,
    sim: &mut SimState,
    log: &mut EventLog,
    runtime: &mut Option<(Instant, sample_clock::SampleClock)>,
    clock_boot: Instant,
    boot_epoch_ns: u64,
    elapsed: Duration,
) {
    let Some(started) = sim.ecg_started_at.filter(|_| sim.ecg_streaming) else {
        *runtime = None;
        return;
    };
    if runtime
        .as_ref()
        .is_none_or(|(generation, _)| *generation != started)
    {
        *runtime = Some((started, sample_clock::SampleClock::new(130)));
    }
    let characteristic = Uuid::parse_str(gatt_spec::pmd::DATA).expect("constant PMD UUID");
    let capacity = match radio.notification_payload_capacity(characteristic).await {
        Ok(Some(capacity)) => capacity,
        Ok(None) => return,
        Err(error) => {
            log.log(
                "radio-error",
                json!({"op": "ecg-notification-capacity", "error": error.to_string()}),
            );
            return;
        }
    };
    let count =
        match gatt_spec::ecg_frame_samples_for_capacity(sim.config.ecg_frame_samples, capacity) {
            Ok(count) => count,
            Err(error) => {
                log.log("ecg-stream-failed", json!({"error": error}));
                sim.apply_pmd_action(PmdAction::StopEcg);
                return;
            }
        };
    let Some((_, clock)) = runtime.as_mut() else {
        return;
    };
    // Preserve the bounded work in samples, not packets: a small writer needs
    // more packets to carry the same acquisition budget at 130 Hz.
    let frame_budget = sim
        .config
        .ecg_frame_samples
        .saturating_mul(32)
        .div_ceil(count);
    for _ in 0..frame_budget {
        let batch = match clock.next(elapsed, count as u64) {
            Ok(Some(batch)) => batch,
            Ok(None) => break,
            Err(error) => {
                log.log("ecg-stream-failed", json!({"error":error}));
                sim.apply_pmd_action(PmdAction::StopEcg);
                break;
            }
        };
        if batch.dropped_samples > 0 {
            log.log(
                "ecg-samples-shed",
                json!({"reason":"host-loop-backlog","samples":batch.dropped_samples}),
            );
        }
        let origin = started.saturating_duration_since(clock_boot);
        let indices = u64::try_from(origin.as_nanos() * 130 / 1_000_000_000)
            .ok()
            .and_then(|offset| offset.checked_add(batch.first))
            .and_then(|first| first.checked_add(count as u64).map(|end| (first, end)));
        let Some((index, end)) = indices else {
            log.log(
                "ecg-stream-failed",
                json!({"error":"ECG waveform sample index exhausted"}),
            );
            sim.apply_pmd_action(PmdAction::StopEcg);
            break;
        };
        sim.ecg_sample_index = end;
        if sim.silent {
            continue;
        }
        // Adversarial constrained delivery: shed frames with a loud line each.
        // The sample index still advances; missing frames leave honest gaps.
        let seq = sim.delivery_seq;
        sim.delivery_seq = sim.delivery_seq.saturating_add(1);
        if !sim::should_deliver(seq, sim.delivery_keep_every) {
            log.log(
                "ecg-frame-shed",
                json!({"seq": seq, "keepEvery": sim.delivery_keep_every}),
            );
            continue;
        }
        let mut samples = Vec::with_capacity(count);
        match &sim.ecg_replay {
            Some(recorded) => ecg::replay_samples(recorded, index, count, &mut samples),
            None => ecg::ecg_frame_samples(index, count, f64::from(sim.config.bpm), &mut samples),
        }
        // Device time, not Unix time: the strap stamps Polar-epoch nanoseconds
        // (or boot-relative time in explicitly unsynchronised mode) of the
        // frame's last sample.
        let timestamp_ns = match sample_clock::sample_timestamp_ns(origin, batch.last_sample_ns)
            .and_then(|elapsed| {
                sim::device_timestamp_from_elapsed_ns(sim.config.clock, boot_epoch_ns, elapsed)
            }) {
            Ok(timestamp) => timestamp,
            Err(error) => {
                log.log("ecg-stream-failed", json!({"error":error}));
                sim.apply_pmd_action(PmdAction::StopEcg);
                break;
            }
        };
        let frame = gatt_spec::encode_ecg_frame(timestamp_ns, &samples);
        let outcome = radio.notify(characteristic, frame.clone()).await;
        report_stream_notify(
            log,
            "ecg-notify",
            outcome,
            json!({"samples": samples.len(), "bytes": frame.len(), "timestampNs": timestamp_ns.to_string()}),
        );
    }
}

/// ACC generation is reset only by a new successful START, not by timer jitter
/// or notification capacity changes. Its clock remains tied to acquisition.
async fn send_acc(
    radio: &mut PlatformRadio,
    sim: &mut SimState,
    log: &mut EventLog,
    runtime: &mut Option<(Instant, acc_stream::Stream)>,
    clock_boot: Instant,
    boot_epoch_ns: u64,
) {
    let (Some(settings), Some(started)) = (sim.acc_settings, sim.acc_started_at) else {
        *runtime = None;
        return;
    };
    if runtime
        .as_ref()
        .is_none_or(|(generation, _)| *generation != started)
    {
        *runtime = Some((started, acc_stream::Stream::new(settings)));
    }
    let characteristic = Uuid::parse_str(gatt_spec::pmd::DATA).expect("constant PMD UUID");
    let capacity = match radio.notification_payload_capacity(characteristic).await {
        Ok(Some(capacity)) => capacity,
        Ok(None) => return,
        Err(error) => {
            log.log(
                "radio-error",
                json!({"op": "acc-notification-capacity", "error": error.to_string()}),
            );
            return;
        }
    };
    let Some((_, stream)) = runtime.as_mut() else {
        return;
    };
    // A paused host may catch up, but cannot monopolize the event loop. The
    // source separately bounds backlog and reports every discarded sample.
    for _ in 0..32 {
        let batch = match stream.next(started.elapsed(), capacity) {
            Ok(Some(batch)) => batch,
            Ok(None) => break,
            Err(error) => {
                log.log("acc-stream-failed", json!({"error": error}));
                sim.apply_pmd_action(PmdAction::StopAcc);
                break;
            }
        };
        if batch.dropped_samples > 0 {
            log.log(
                "acc-samples-shed",
                json!({"reason": "host-loop-backlog", "samples": batch.dropped_samples}),
            );
        }
        if sim.silent {
            continue;
        }
        let timestamp = sample_clock::sample_timestamp_ns(
            started.saturating_duration_since(clock_boot),
            batch.last_sample_ns,
        )
        .and_then(|elapsed| {
            sim::device_timestamp_from_elapsed_ns(sim.config.clock, boot_epoch_ns, elapsed)
        });
        let frame = timestamp.and_then(|timestamp| {
            acc::frame(timestamp, &batch.samples).map(|frame| (timestamp, frame))
        });
        match frame {
            Ok((timestamp, frame)) => {
                let bytes = frame.len();
                let outcome = radio.notify(characteristic, frame).await;
                report_stream_notify(
                    log,
                    "acc-notify",
                    outcome,
                    json!({"samples": batch.samples.len(), "bytes": bytes, "timestampNs": timestamp.to_string(), "sampleRateHz": settings.sample_rate_hz, "rangeG": settings.range_g}),
                );
            }
            Err(error) => {
                log.log("acc-stream-failed", json!({"error": error}));
                sim.apply_pmd_action(PmdAction::StopAcc);
                break;
            }
        }
    }
}

/// Log line for one settled send. Every settle outcome is logged: a queued
/// frame's `ecg-notify` line is a request, and its `notify-settled` line is
/// the answer — counting queued lines as deliveries overcounts whenever a
/// frame settles late or fails. Failures stay loud as `radio-error`.
/// Pure so the every-settle-is-visible invariant is unit-pinned.
fn notify_settled_log(
    id: u64,
    service: &str,
    characteristic: &str,
    outcome: &SendOutcome,
) -> (&'static str, serde_json::Value) {
    match outcome {
        SendOutcome::OsAccepted => (
            "notify-settled",
            json!({"id": id, "service": service, "characteristic": characteristic, "outcome": "os-accepted"}),
        ),
        SendOutcome::Queued { .. } => (
            "notify-settled",
            json!({"service": service, "characteristic": characteristic, "outcome": "queued", "id": id}),
        ),
        SendOutcome::NotSubscribed => (
            "radio-error",
            json!({"id": id, "op": "notify-settled", "service": service, "characteristic": characteristic, "error": "session ended before delivery"}),
        ),
        SendOutcome::Failed(reason) => (
            "radio-error",
            json!({"id": id, "op": "notify-settled", "service": service, "characteristic": characteristic, "error": reason}),
        ),
    }
}

fn slice_at(value: &[u8], offset: u64) -> Option<Vec<u8>> {
    let offset_usize: usize = offset.try_into().ok()?;
    value.get(offset_usize..).map(<[u8]>::to_vec)
}

async fn handle_radio(
    event: RadioEvent,
    radio: &mut PlatformRadio,
    sim: &mut SimState,
    timing: &mut timing::TimingRuntime,
    log: &mut EventLog,
    psftp_session: &mut psftp::Session,
    _boot_epoch_ns: u64,
) {
    match event {
        RadioEvent::Powered(powered) => {
            if !powered {
                sim.reset_pmd_session();
                psftp_session.reset();
                let psftp = psftp::wire_characteristic();
                if let Err(error) = radio.drop_subscription(psftp).await {
                    log.log(
                        "psftp-subscription-reset-failed",
                        json!({"reason": error.to_string()}),
                    );
                }
                log.log_simple("pmd-session-ended");
            }
            log.log("powered", json!({"on": powered}));
        }
        RadioEvent::Subscription {
            service,
            characteristic,
            subscribed,
        } => {
            if characteristic.eq_ignore_ascii_case(gatt_spec::feee::CHAR_51) {
                psftp_session.reset();
            }
            sim.observe_subscription(&characteristic, subscribed);
            if !subscribed && characteristic.eq_ignore_ascii_case(gatt_spec::pmd::DATA) {
                log.log("pmd-session-ended", json!({"reason": "final-pmd-subscription-ended", "characteristic": characteristic}));
            }
            log.log(
                if subscribed {
                    "subscribed"
                } else {
                    "unsubscribed"
                },
                json!({"service": service, "characteristic": characteristic}),
            );
            // Adversarial interrupted setup: an armed fault tears the new
            // session down immediately, so the central's subscribe completes
            // but no stream ever delivers on it. The fault is recorded here,
            // when it manifests — not when it was armed.
            if subscribed && sim.interrupt_next_subscribe {
                sim.interrupt_next_subscribe = false;
                let uuid = Uuid::parse_str(&characteristic).unwrap_or_else(|_| Uuid::nil());
                match radio.drop_subscription(uuid).await {
                    Ok(torn) => {
                        sim.record_fault(
                            "interrupt-next-subscribe",
                            json!({"characteristic": characteristic, "torn": torn}),
                        );
                        log.log(
                            "subscribe-interrupted",
                            json!({"service": service, "characteristic": characteristic, "torn": torn}),
                        );
                    }
                    Err(error) => {
                        log.log(
                            "radio-error",
                            json!({"op": "interrupt-next-subscribe", "error": error.to_string()}),
                        );
                    }
                }
            }
        }
        RadioEvent::IndicationConfirmed {
            service,
            characteristic,
            receipt_id,
            generation,
        } => {
            log.log(
                "indication-confirmed",
                json!({"service": service, "characteristic": characteristic, "receiptId": receipt_id, "generation": generation}),
            );
        }
        RadioEvent::IndicationUnconfirmed {
            service,
            characteristic,
            receipt_id,
            generation,
            reason,
        } => {
            log.log(
                "indication-unconfirmed",
                json!({"service": service, "characteristic": characteristic, "receiptId": receipt_id, "generation": generation, "reason": reason}),
            );
        }
        RadioEvent::Read {
            service,
            characteristic,
            offset,
            reply,
        } => {
            let uuid = Uuid::parse_str(&characteristic).unwrap_or_else(|_| Uuid::nil());
            let answer = read_answer(sim, &uuid, offset);
            log.log(
                "read",
                json!({"service": service, "characteristic": characteristic, "offset": offset, "ok": answer.ok, "len": answer.value.len()}),
            );
            let _ = reply.send(answer);
        }
        RadioEvent::NotifySettled {
            id,
            service,
            characteristic,
            outcome,
        } => {
            if characteristic.eq_ignore_ascii_case(gatt_spec::pmd::CONTROL_POINT) {
                if let Some(action) =
                    sim.settle_pmd_action(id, matches!(outcome, SendOutcome::OsAccepted))
                {
                    commit_pmd_action(sim, log, action);
                }
            }
            sim.observe_notify_settled(&characteristic, &outcome);
            let (kind, detail) = notify_settled_log(id, &service, &characteristic, &outcome);
            log.log(kind, detail);
        }
        RadioEvent::Write {
            service,
            characteristic,
            value,
            device_address,
            owned_target,
            reply,
        } => {
            let uuid = Uuid::parse_str(&characteristic).unwrap_or_else(|_| Uuid::nil());
            let accepted = if psftp::is_wire_request(uuid) {
                handle_psftp_write(
                    radio,
                    log,
                    psftp_session,
                    &service,
                    &value,
                    device_address.as_deref(),
                    owned_target.as_ref(),
                )
                .await
            } else {
                write_request(radio, sim, timing, log, &service, &uuid, &value).await
            };
            log.log(
                "write",
                json!({"service": service, "characteristic": characteristic, "len": value.len(), "accepted": accepted}),
            );
            let _ = reply.send(accepted);
        }
    }
}

fn read_answer(sim: &SimState, uuid: &Uuid, offset: u64) -> radio::RadioReadAnswer {
    use radio::RadioReadAnswer;
    let full: Option<Vec<u8>> = if *uuid
        == Uuid::parse_str(gatt_spec::pmd::CONTROL_POINT).unwrap_or_else(|_| Uuid::nil())
    {
        Some(sim.pmd_features())
    } else {
        match short_of(uuid) {
            Some(short) => sim.static_read(short),
            None => sim.vendor_read(uuid),
        }
    };
    match full.and_then(|value| slice_at(&value, offset)) {
        Some(value) => RadioReadAnswer { value, ok: true },
        None => RadioReadAnswer {
            value: Vec::new(),
            ok: false,
        },
    }
}

async fn write_request(
    radio: &mut PlatformRadio,
    sim: &mut SimState,
    timing: &mut timing::TimingRuntime,
    log: &mut EventLog,
    service: &str,
    uuid: &Uuid,
    value: &[u8],
) -> bool {
    let control_point =
        Uuid::parse_str(gatt_spec::pmd::CONTROL_POINT).unwrap_or_else(|_| Uuid::nil());
    if *uuid != control_point {
        // Writable vendor characteristics (6217ff4d, FEEE 0x53) have no
        // behaviour model: the write is refused loudly, never absorbed.
        let reason = if sim.vendor_writable(uuid) {
            "unmodeled-vendor-write"
        } else {
            "not-writable"
        };
        log.log(
            "write-rejected",
            json!({"service": service, "reason": reason}),
        );
        return false;
    }
    if !sim.can_admit_pmd_command() {
        log.log(
            "pmd-command-rejected",
            json!({"reason": "pending-command-capacity", "capacity": radio::SEND_QUEUE_CAPACITY}),
        );
        return false;
    }
    let outcome = sim.handle_pmd_write(value);
    match outcome.indicate {
        Some(response) => {
            let status = response.get(3).copied().unwrap_or(0xFF);
            log.log(
                "pmd-command",
                json!({"op": value.first().copied().unwrap_or(0xFF), "status": status, "action": format!("{:?}", outcome.action)}),
            );
            // Remember the response bytes: adversarial `stale-callback`
            // replays them out of sequence.
            sim.last_pmd_response = Some(response.clone());
            // A measured response latency defers the indication to the tick
            // loop; the ATT write is answered now, so the loop never sleeps.
            // Immediate by default, so behaviour is unchanged. The
            // adversarial `delay-responses` fault adds its extra latency on
            // top through the same deferred path — still off the loop.
            // Either way the streaming action commits only when the
            // indication actually goes out — never frames before the
            // START/STOP response.
            let measured_ms = timing.sample_pmd_response_ms();
            let injected_ms = sim.response_delay_ms;
            if measured_ms.is_some() || injected_ms > 0 || !sim.pending_indications.is_empty() {
                let total_ms = measured_ms.unwrap_or(0.0) + injected_ms as f64;
                sim.pending_indications.push(sim::PendingIndication {
                    due: Instant::now() + Duration::from_secs_f64(total_ms / 1000.0),
                    response,
                    action: outcome.action,
                });
                log.log(
                    "pmd-indicate-deferred",
                    json!({"delayMs": total_ms, "injectedMs": injected_ms}),
                );
                return true;
            }
            // Inline: a lost indication surfaces here instead of timing out
            // the central 5 s later.
            match radio.notify(control_point, response.clone()).await {
                Ok(SendOutcome::OsAccepted) => {
                    commit_pmd_action(sim, log, outcome.action);
                    true
                }
                Ok(SendOutcome::Queued { id }) => {
                    sim.queue_pmd_action(id, outcome.action);
                    true
                }
                Ok(SendOutcome::NotSubscribed) => {
                    log.log(
                        "radio-error",
                        json!({"op": "pmd-indicate", "error": "no live indication session"}),
                    );
                    false
                }
                Ok(SendOutcome::Failed(reason)) => {
                    log.log(
                        "radio-error",
                        json!({"op": "pmd-indicate", "error": reason}),
                    );
                    false
                }
                Err(error) => {
                    log.log(
                        "radio-error",
                        json!({"op": "pmd-indicate", "error": error.to_string()}),
                    );
                    false
                }
            }
        }
        None => {
            log.log(
                "write-rejected",
                json!({"service": service, "reason": "malformed-pmd-command"}),
            );
            false
        }
    }
}

async fn handle_psftp_write(
    radio: &mut PlatformRadio,
    log: &mut EventLog,
    session: &mut psftp::Session,
    service: &str,
    value: &[u8],
    device_address: Option<&str>,
    target: Option<&radio::OwnedNotifyTarget>,
) -> bool {
    let Some(target) = target else {
        log.log(
            "psftp-response-dropped",
            json!({"reason": "exact D2H writer identity is unavailable", "deviceAddress": device_address}),
        );
        session.reset();
        return false;
    };
    match session.ingest_for(target, value) {
        Ok(None) => {
            log.log(
                "psftp-fragment",
                json!({"service": service, "len": value.len()}),
            );
            true
        }
        Ok(Some(request)) => {
            let d2h = psftp::wire_response_characteristic();
            let response = psftp::not_implemented_response().to_vec();
            log.log(
                "psftp-request",
                json!({"service": service, "requestBytes": request.len(), "status": 201, "responseBytes": response.len()}),
            );
            match radio.notify_to(d2h, response, target.clone()).await {
                Ok(SendOutcome::OsAccepted) | Ok(SendOutcome::Queued { .. }) => true,
                Ok(SendOutcome::NotSubscribed) => {
                    log.log(
                        "psftp-response-dropped",
                        json!({"reason": "D2H characteristic is not subscribed"}),
                    );
                    false
                }
                Ok(SendOutcome::Failed(reason)) => {
                    log.log("psftp-response-dropped", json!({"reason": reason}));
                    false
                }
                Err(error) => {
                    log.log(
                        "psftp-response-dropped",
                        json!({"reason": error.to_string()}),
                    );
                    false
                }
            }
        }
        Err(error) => {
            log.log(
                "psftp-frame-rejected",
                json!({"error": format!("{error:?}")}),
            );
            false
        }
    }
}

/// Commits a decided PMD action at indication time, announcing stream
/// transitions the moment they become visible over the air.
fn commit_pmd_action(sim: &mut SimState, log: &mut EventLog, action: PmdAction) {
    sim.apply_pmd_action(action);
    match action {
        PmdAction::StartEcg => log.log_simple("ecg-started"),
        PmdAction::StopEcg => log.log_simple("ecg-stopped"),
        PmdAction::StartAcc(settings) => log.log("acc-started", json!({"sampleRateHz": settings.sample_rate_hz, "resolutionBits": acc::RESOLUTION_BITS, "rangeG": settings.range_g})),
        PmdAction::StopAcc => log.log_simple("acc-stopped"),
        PmdAction::None => {}
    }
}

/// Sends PMD indications whose latency expired. The streaming action commits
/// only when the indication actually goes out; a central that vanished
/// mid-latency drops the indication loudly and leaves no stuck stream.
async fn drain_indications(radio: &mut PlatformRadio, sim: &mut SimState, log: &mut EventLog) {
    let now = Instant::now();
    // Responses and their state changes stay in command order even when
    // injected/measured latencies differ between consecutive commands.
    while let Some(indication) = sim.take_due_indication(now) {
        let control_point =
            Uuid::parse_str(gatt_spec::pmd::CONTROL_POINT).unwrap_or_else(|_| Uuid::nil());
        let outcome = radio.notify(control_point, indication.response).await;
        match &outcome {
            Ok(SendOutcome::OsAccepted) => {
                commit_pmd_action(sim, log, indication.action);
            }
            Ok(SendOutcome::Queued { id }) => {
                sim.queue_pmd_action(*id, indication.action);
            }
            Ok(SendOutcome::NotSubscribed) => {
                log.log(
                    "pmd-indicate-dropped",
                    json!({"reason": "central left before the deferred indication went out; action not committed"}),
                );
            }
            _ => {}
        }
        report_stream_notify(log, "pmd-indicate", outcome, json!({"deferred": true}));
    }
}

async fn handle_control(
    request: ControlRequest,
    radio: &mut PlatformRadio,
    sim: &mut SimState,
    log: &mut EventLog,
) {
    let ControlRequest { command, reply } = request;
    log.log(
        "control-command",
        json!({"command": format!("{command:?}")}),
    );
    // Faithful mode reproduces the captured strap and injects nothing: an
    // adversarial fault command is refused loudly here, before it can touch
    // any state — never a silent no-op.
    if let Err(error) = control::check_mode(sim.run_mode, &command) {
        log.log(
            "control-refused",
            json!({"command": command.name(), "mode": sim.run_mode.as_str()}),
        );
        let _ = reply.send(ControlReply::failed(error));
        return;
    }
    let answer = match command {
        ControlCommand::SetBpm { bpm } => {
            sim.config.bpm = bpm;
            // An explicit bpm wins over the scripted curve.
            sim.config.bpm_curve.clear();
            ControlReply::ok()
        }
        ControlCommand::SetAdvertising { on } => match set_advertising(radio, sim, log, on).await {
            Ok(reply) => reply,
            Err(error) => ControlReply::failed(error),
        },
        ControlCommand::DropLink => match drop_link(radio, sim, log).await {
            Ok(reply) => reply,
            Err(error) => ControlReply::failed(error),
        },
        ControlCommand::FlapLink => match flap_link(radio, sim, log).await {
            Ok(reply) => reply,
            Err(error) => ControlReply::failed(error),
        },
        ControlCommand::SetSilent { on } => {
            sim.silent = on;
            sim.record_fault("set-silent", json!({"on": on}));
            log.log("silent", json!({"on": on}));
            ControlReply::ok()
        }
        ControlCommand::DelayResponses { ms } => {
            if ms > 10_000 {
                ControlReply::failed(format!("delay-responses ms {ms} is out of range 0..=10000"))
            } else {
                sim.response_delay_ms = ms;
                sim.record_fault("delay-responses", json!({"ms": ms}));
                log.log("response-delay", json!({"ms": ms}));
                ControlReply::ok()
            }
        }
        ControlCommand::InterruptNextSubscribe => {
            // Armed here (logged); the fault itself is recorded when the
            // next subscription fires it — one record entry per manifested
            // fault.
            sim.interrupt_next_subscribe = true;
            log.log("subscribe-interrupt-armed", json!({}));
            ControlReply::ok()
        }
        ControlCommand::StaleCallback => match stale_callback(radio, sim, log).await {
            Ok(reply) => reply,
            Err(error) => ControlReply::failed(error),
        },
        ControlCommand::ConstrainDelivery { keep_every } => {
            if !(1..=1000).contains(&keep_every) {
                ControlReply::failed(format!(
                    "constrain-delivery keepEvery {keep_every} is out of range 1..=1000"
                ))
            } else {
                sim.delivery_keep_every = keep_every;
                sim.record_fault("constrain-delivery", json!({"keepEvery": keep_every}));
                log.log("delivery-constrained", json!({"keepEvery": keep_every}));
                ControlReply::ok()
            }
        }
        ControlCommand::RunRecord => ControlReply {
            ok: true,
            error: None,
            note: None,
            state: Some(sim.run_record()),
        },
        ControlCommand::SetBattery { level } => {
            if level > 100 {
                ControlReply::failed(format!("battery level {level} is out of range 0..=100"))
            } else {
                sim.config.battery_percent = level;
                sim.battery_carry = 0.0;
                log.log("battery-set", json!({"percent": level}));
                ControlReply::ok()
            }
        }
        ControlCommand::SetContact { detected } => {
            sim.config.contact_detected = detected;
            log.log("contact", json!({"detected": detected}));
            ControlReply::ok()
        }
        ControlCommand::PairPolicy { policy } => {
            sim.config.pair_policy = policy;
            log.log("pair-policy", json!({"policy": policy.as_str()}));
            ControlReply::ok()
        }
        ControlCommand::LoadProfile { path } => {
            match load_profile_into(sim, radio, log, &path).await {
                Ok(()) => ControlReply::ok(),
                Err(error) => ControlReply::failed(error),
            }
        }
        ControlCommand::RejectNextPmd { status } => {
            sim.reject_next_status = Some(status);
            sim.record_fault("reject-next-pmd", json!({"status": status}));
            log.log("pmd-fault-armed", json!({"status": status}));
            ControlReply::ok()
        }
        ControlCommand::ClearPmdFault => {
            sim.reject_next_status = None;
            sim.record_fault("clear-pmd-fault", json!({}));
            log.log("pmd-fault-cleared", json!({}));
            ControlReply::ok()
        }
        ControlCommand::SetRates {
            hr_hz,
            ecg_frames_per_sec,
            ecg_frame_samples,
        } => match apply_rates(
            &mut sim.config,
            hr_hz,
            ecg_frames_per_sec,
            ecg_frame_samples,
        ) {
            Ok(()) => ControlReply::ok(),
            Err(error) => ControlReply::failed(error),
        },
        ControlCommand::GetState => ControlReply {
            ok: true,
            error: None,
            note: None,
            state: Some(sim.snapshot()),
        },
        ControlCommand::Help => ControlReply {
            ok: true,
            error: None,
            note: Some(control::command_help()),
            state: None,
        },
    };
    let _ = reply.send(answer);
}

/// Loads a profile file onto the live sim, transactionally: the old config
/// and replays are snapshotted first, and a radio failure after the swap
/// rolls everything back and reports the degraded state — never a new config
/// on a failed radio. A bad path or replay file keeps the old profile and
/// answers `{"ok":false}`. When the radio is advertising, the advertisement
/// is re-registered so the new name takes effect over the air.
async fn load_profile_into(
    sim: &mut SimState,
    radio: &mut impl PeripheralRadio,
    log: &mut EventLog,
    path: &str,
) -> Result<(), String> {
    let loaded = profile::load_profile(path)?;
    // Resolve the replay files before swapping the config: a bad file keeps
    // the old profile instead of a half-applied one.
    let replay = load_ecg_replay(&loaded.pmd.ecg_source)?;
    let hr_replay = load_hr_replay(&loaded.heart_rate.hr_source)?;
    let old_config = sim.config.clone();
    let old_ecg_replay = sim.ecg_replay.clone();
    let old_hr_replay = sim.hr_replay.clone();
    let old_hr_replay_index = sim.hr_replay_index;
    let old_pending = sim.pending_indications.clone();
    sim.config.apply_profile(path, &loaded)?;
    sim.ecg_replay = replay;
    sim.hr_replay = hr_replay;
    sim.hr_replay_index = 0;
    sim.pending_indications.clear();
    let radio_outcome = match radio.is_advertising().await {
        Ok(true) => start_advertising(radio, sim, log).await,
        Ok(false) => Ok(()),
        Err(error) => {
            log.log(
                "radio-error",
                json!({"op": "is-advertising", "error": error.to_string()}),
            );
            Err(error.to_string())
        }
    };
    if let Err(error) = radio_outcome {
        // The new config is already swapped in: roll it back so the sim
        // never runs a profile its radio rejected, and say so loudly.
        sim.config = old_config;
        sim.ecg_replay = old_ecg_replay;
        sim.hr_replay = old_hr_replay;
        sim.hr_replay_index = old_hr_replay_index;
        sim.pending_indications = old_pending;
        log.log("profile-rolled-back", json!({"path": path, "error": error}));
        return Err(error);
    }
    log.log("profile-loaded", json!({"path": path}));
    Ok(())
}

async fn set_advertising(
    radio: &mut PlatformRadio,
    sim: &SimState,
    log: &mut EventLog,
    on: bool,
) -> Result<ControlReply, String> {
    if on {
        start_advertising(radio, sim, log).await?;
        Ok(ControlReply::ok())
    } else {
        let detail = radio.advertising_detail();
        radio
            .stop_advertising()
            .await
            .map_err(|error| error.to_string())?;
        log.log("advertising-stopped", json!({"advertising": detail}));
        Ok(ControlReply::ok())
    }
}

/// Builds the drop-link note and state from the tracked clients, the
/// allowlist, the disconnect report and the connected strangers. Pure so
/// the report wording is pinned by tests without a radio.
fn summarize_drop(
    clients: &[String],
    allowlist: &[String],
    report: &radio::DisconnectReport,
    connected: &[String],
) -> (String, serde_json::Value) {
    let targets = radio::drop_targets(clients, allowlist);
    let mut skipped = report.skipped.clone();
    for stranger in radio::non_client_skips(connected, &targets) {
        skipped.push(radio::DisconnectSkip {
            address: stranger,
            reason: radio::SKIP_NOT_CLIENT.to_string(),
        });
    }
    let state = json!({
        "clients": clients,
        "allowlisted": allowlist.len(),
        "targets": targets,
        "dropped": report.dropped,
        "skipped": skipped,
    });
    let note = if targets.is_empty() {
        "ECG halted; no simulator clients observed and no drop-link allowlist \
         is configured (--drop-link-allow), so no central was disconnected — \
         streams stop but any peer stays connected. Advertising and the GATT \
         database are unchanged"
            .to_string()
    } else {
        let mut note = format!(
            "ECG halted, {} of {} target(s) disconnected",
            report.dropped.len(),
            targets.len(),
        );
        if !report.dropped.is_empty() {
            note.push_str(&format!(" ({})", report.dropped.join(", ")));
        }
        for skip in &skipped {
            note.push_str(&format!("; {} skipped: {}", skip.address, skip.reason));
        }
        note.push_str(
            "; advertising and the GATT database are unchanged (BlueZ \
             disconnects via Device1.Disconnect; on CoreBluetooth an \
             already-connected central stays connected until it disconnects)",
        );
        note
    };
    (note, state)
}

async fn drop_link(
    radio: &mut PlatformRadio,
    sim: &mut SimState,
    log: &mut EventLog,
) -> Result<ControlReply, String> {
    // A simulated link loss drops the link, not the peripheral: advertising
    // and the GATT database stay exactly as they were, so centrals see a
    // lifecycle loss (peer-link-loss) with no Service Changed, and can
    // reconnect immediately — like walking back into range of a real H10.
    sim.reset_pmd_session();
    // Only the sim's own clients go: centrals whose addresses touched this
    // peripheral's GATT application, plus the explicit `--drop-link-allow`
    // extras. The adapter's other devices are never disturbed; the other
    // isolation option is a dedicated adapter. CoreBluetooth has no
    // force-disconnect API, so a connected central stays up there either way.
    let clients = radio.simulator_clients();
    let allowlist = sim.config.drop_link_allowlist.clone();
    let targets = radio::drop_targets(&clients, &allowlist);
    let report = radio
        .disconnect_centrals(&targets)
        .await
        .map_err(|error| error.to_string())?;
    // Connected devices that are neither clients nor allowlisted stay up;
    // the report names each one with its reason instead of silently
    // leaving it out. An enumeration failure degrades that part loudly —
    // the drops above already happened and are reported regardless.
    let connected = match radio.connected_devices().await {
        Ok(connected) => connected,
        Err(error) => {
            log.log(
                "radio-error",
                json!({"op": "connected-devices", "error": error.to_string()}),
            );
            Vec::new()
        }
    };
    let (note, state) = summarize_drop(&clients, &allowlist, &report, &connected);
    sim.record_fault(
        "drop-link",
        json!({"dropped": report.dropped, "targets": targets}),
    );
    log.log(
        "link-dropped",
        json!({
            "disconnected": report.dropped.len(),
            "clients": clients,
            "allowlisted": allowlist.len(),
            "skipped": state["skipped"],
        }),
    );
    Ok(ControlReply {
        ok: true,
        error: None,
        note: Some(note),
        state: Some(state),
    })
}

/// Adversarial rapid reconnect: drops the client links like drop-link, then
/// bounces advertising (stop + start) so centrals run a full
/// disconnect/reconnect cycle instead of resuming on the live advertisement.
async fn flap_link(
    radio: &mut PlatformRadio,
    sim: &mut SimState,
    log: &mut EventLog,
) -> Result<ControlReply, String> {
    let mut reply = drop_link(radio, sim, log).await?;
    radio
        .stop_advertising()
        .await
        .map_err(|error| error.to_string())?;
    log.log_simple("advertising-bounced");
    start_advertising(radio, sim, log).await?;
    sim.record_fault("flap-link", json!({}));
    log.log("link-flapped", json!({}));
    if let Some(note) = reply.note.take() {
        reply.note = Some(format!("{note}; advertising bounced for a rapid reconnect"));
    }
    Ok(reply)
}

/// Adversarial stale callback: re-notifies the last PMD response out of
/// sequence. Refuses loudly when no PMD response has gone out yet — there
/// is nothing stale to replay, and silence would lie about it.
async fn stale_callback(
    radio: &mut PlatformRadio,
    sim: &mut SimState,
    log: &mut EventLog,
) -> Result<ControlReply, String> {
    let Some(response) = sim.last_pmd_response.clone() else {
        return Err("stale-callback refused: no PMD response has gone out yet".to_string());
    };
    let control_point =
        Uuid::parse_str(gatt_spec::pmd::CONTROL_POINT).unwrap_or_else(|_| Uuid::nil());
    let outcome = radio.notify(control_point, response.clone()).await;
    let accepted = matches!(
        &outcome,
        Ok(SendOutcome::OsAccepted) | Ok(SendOutcome::Queued { .. })
    );
    sim.record_fault(
        "stale-callback",
        json!({"bytes": response.len(), "accepted": accepted}),
    );
    log.log(
        "stale-callback",
        json!({"bytes": response.len(), "accepted": accepted}),
    );
    report_stream_notify(
        log,
        "stale-callback-notify",
        outcome,
        json!({"bytes": response.len()}),
    );
    Ok(ControlReply::ok_note(format!(
        "replayed the last PMD response ({} bytes) out of sequence; delivery {}",
        response.len(),
        if accepted { "accepted" } else { "failed" },
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn psftp_request_and_response_use_feee_char51_wire_route() {
        let services = h10_services(&SimConfig::default()).expect("H10 surface builds");
        let feee = services
            .iter()
            .find(|service| {
                service.uuid == advertisement::short_uuid(gatt_spec::uuid16::POLAR_ADV_SERVICE)
            })
            .expect("Polar FEEE service");
        let char51 = feee
            .characteristics
            .iter()
            .find(|characteristic| {
                characteristic
                    .uuid
                    .to_string()
                    .eq_ignore_ascii_case(gatt_spec::feee::CHAR_51)
            })
            .expect("FEEE CHAR_51");
        assert!(char51.properties.contains(&radio::CharProperty::Write));
        assert!(char51
            .properties
            .contains(&radio::CharProperty::WriteWithoutResponse));
        assert!(char51.properties.contains(&radio::CharProperty::Notify));

        let char52 = Uuid::parse_str(gatt_spec::feee::CHAR_52).expect("CHAR_52");
        let char53 = Uuid::parse_str(gatt_spec::feee::CHAR_53).expect("CHAR_53");
        assert_eq!(psftp::wire_characteristic(), char51.uuid);
        assert!(psftp::is_wire_request(char51.uuid));
        assert!(!psftp::is_wire_request(char52));
        assert!(!psftp::is_wire_request(char53));
        assert_eq!(psftp::wire_response_characteristic(), char51.uuid);
    }

    struct EcgRadio {
        capacity: Result<Option<usize>, String>,
        frames: Vec<Vec<u8>>,
    }

    #[async_trait::async_trait]
    impl PeripheralRadio for EcgRadio {
        async fn open(_: mpsc::Sender<RadioEvent>) -> Result<Self, radio::RadioError> {
            unreachable!()
        }
        async fn is_powered(&mut self) -> Result<bool, radio::RadioError> {
            unreachable!()
        }
        async fn is_advertising(&mut self) -> Result<bool, radio::RadioError> {
            unreachable!()
        }
        async fn start_advertising(
            &mut self,
            _: &str,
            _: &[Uuid],
        ) -> Result<(), radio::RadioError> {
            unreachable!()
        }
        async fn stop_advertising(&mut self) -> Result<(), radio::RadioError> {
            unreachable!()
        }
        async fn add_service(&mut self, _: &radio::ServiceSpec) -> Result<(), radio::RadioError> {
            unreachable!()
        }
        async fn notification_payload_capacity(
            &self,
            characteristic: Uuid,
        ) -> Result<Option<usize>, radio::RadioError> {
            assert_eq!(
                characteristic.to_string(),
                gatt_spec::pmd::DATA.to_lowercase()
            );
            self.capacity.clone().map_err(radio::RadioError)
        }
        async fn notify(
            &mut self,
            characteristic: Uuid,
            frame: Vec<u8>,
        ) -> Result<SendOutcome, radio::RadioError> {
            assert_eq!(
                characteristic.to_string(),
                gatt_spec::pmd::DATA.to_lowercase()
            );
            let bytes = frame.len();
            self.frames.push(frame);
            match &self.capacity {
                Ok(Some(capacity)) if bytes <= *capacity => Ok(SendOutcome::OsAccepted),
                _ => Err(radio::RadioError("data length exceeds MTU".into())),
            }
        }
    }

    #[tokio::test]
    async fn ecg_notifications_fit_writer_capacity() {
        let mut sim = SimState::new(SimConfig::default());
        let started = Instant::now() - Duration::from_secs(1);
        sim.ecg_streaming = true;
        sim.ecg_started_at = Some(started);
        let mut radio = EcgRadio {
            capacity: Ok(Some(18)),
            frames: Vec::new(),
        };
        send_ecg(
            &mut radio,
            &mut sim,
            &mut EventLog::new(),
            &mut None,
            started - Duration::from_secs(10),
            sim::POLAR_EPOCH_OFFSET_NS + 5_000_000_000,
        )
        .await;
        assert!(
            !radio.frames.is_empty(),
            "small MTU must still deliver acquired ECG samples"
        );
        assert!(
            radio.frames.iter().all(|frame| frame.len() <= 18),
            "legal writer capacity18 must not receive the configured229-byte frame: {:?}",
            radio.frames.iter().map(Vec::len).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn ecg_normal_small_and_exact_boundary_packets_preserve_samples_and_time() {
        for (capacity, expected_count) in [
            (13, 1),
            (15, 1),
            (16, 2),
            (18, 2),
            (228, 72),
            (229, 73),
            (244, 73),
            (511, 73),
        ] {
            for recorded in [false, true] {
                let mut sim = SimState::new(SimConfig::default());
                if recorded {
                    sim.ecg_replay = Some(vec![7, -8, 9]);
                }
                let started = Instant::now() - Duration::from_secs(1);
                sim.ecg_streaming = true;
                sim.ecg_started_at = Some(started);
                let mut radio = EcgRadio {
                    capacity: Ok(Some(capacity)),
                    frames: Vec::new(),
                };
                send_ecg_at(
                    &mut radio,
                    &mut sim,
                    &mut EventLog::new(),
                    &mut None,
                    started - Duration::from_secs(10),
                    sim::POLAR_EPOCH_OFFSET_NS + 5_000_000_000,
                    Duration::from_secs(1),
                )
                .await;
                assert!(!radio.frames.is_empty());
                let mut delivered = 0;
                for frame in &radio.frames {
                    assert!(frame.len() <= capacity);
                    let count = (frame.len() - 10) / 3;
                    assert_eq!(count, expected_count);
                    let mut expected_samples = Vec::new();
                    let index = 1300 + delivered;
                    if let Some(samples) = &sim.ecg_replay {
                        ecg::replay_samples(samples, index, count, &mut expected_samples);
                    } else {
                        ecg::ecg_frame_samples(
                            index,
                            count,
                            f64::from(sim.config.bpm),
                            &mut expected_samples,
                        );
                    }
                    delivered += count as u64;
                    let timestamp = 15_000_000_000 + (delivered - 1) * 1_000_000_000 / 130;
                    assert_eq!(
                        *frame,
                        gatt_spec::encode_ecg_frame(timestamp, &expected_samples)
                    );
                }
                assert_eq!(sim.ecg_sample_index, 1300 + delivered);
                assert_eq!(sim.delivery_seq, radio.frames.len() as u64);
                assert!(sim.ecg_streaming);
            }
        }
    }

    #[tokio::test]
    async fn ecg_capacity_changes_keep_acquisition_and_delivery_identity() {
        let mut sim = SimState::new(SimConfig::default());
        let started = Instant::now() - Duration::from_secs(1);
        sim.ecg_streaming = true;
        sim.ecg_started_at = Some(started);
        let mut radio = EcgRadio {
            capacity: Ok(Some(18)),
            frames: Vec::new(),
        };
        let mut runtime = None;
        let mut log = EventLog::new();
        send_ecg_at(
            &mut radio,
            &mut sim,
            &mut log,
            &mut runtime,
            started - Duration::from_secs(10),
            sim::POLAR_EPOCH_OFFSET_NS + 5_000_000_000,
            Duration::from_millis(500),
        )
        .await;
        assert_eq!(radio.frames.len(), 32);
        assert_eq!(sim.ecg_sample_index, 1364);
        radio.capacity = Ok(Some(13));
        send_ecg_at(
            &mut radio,
            &mut sim,
            &mut log,
            &mut runtime,
            started - Duration::from_secs(10),
            sim::POLAR_EPOCH_OFFSET_NS + 5_000_000_000,
            Duration::from_millis(600),
        )
        .await;
        assert_eq!(radio.frames.len(), 46);
        assert_eq!(sim.ecg_sample_index, 1378);
        assert_eq!(sim.delivery_seq, 46);
        assert_eq!(
            u64::from_le_bytes(radio.frames[32][1..9].try_into().unwrap()),
            15_000_000_000 + 64 * 1_000_000_000 / 130
        );
    }

    #[tokio::test]
    async fn ecg_legal_small_writer_preserves_130_hz_across_normal_dispatch() {
        for capacity in [13, 18, 229] {
            let mut sim = SimState::new(SimConfig::default());
            let started = Instant::now();
            sim.ecg_streaming = true;
            sim.ecg_started_at = Some(started);
            let mut radio = EcgRadio {
                capacity: Ok(Some(capacity)),
                frames: Vec::new(),
            };
            let mut runtime = None;
            let mut log = EventLog::new();
            let mut delivered = 0;
            for tick in 1..=100 {
                // The normal 73/130-second cadence, rounded up by <1 ns.
                let due = tick * 73_u64;
                let elapsed = Duration::from_nanos((due * 1_000_000_000).div_ceil(130));
                let previous = radio.frames.len();
                send_ecg_at(
                    &mut radio,
                    &mut sim,
                    &mut log,
                    &mut runtime,
                    started - Duration::from_secs(10),
                    sim::POLAR_EPOCH_OFFSET_NS + 5_000_000_000,
                    elapsed,
                )
                .await;
                for frame in &radio.frames[previous..] {
                    assert!(frame.len() <= capacity);
                    delivered += ((frame.len() - 10) / 3) as u64;
                    assert_eq!(
                        u64::from_le_bytes(frame[1..9].try_into().unwrap()),
                        15_000_000_000 + (delivered - 1) * 1_000_000_000 / 130
                    );
                }
                let effective =
                    gatt_spec::ecg_frame_samples_for_capacity(73, capacity).unwrap() as u64;
                assert!(
                    due - delivered < effective,
                    "no artificial 32-packet throttle"
                );
                assert_eq!(sim.ecg_sample_index, 1300 + delivered);
                if capacity == 13 {
                    assert_eq!(radio.frames.len() - previous, 73);
                }
            }
            assert_eq!(delivered, 7300);
            assert_eq!(sim.delivery_seq, radio.frames.len() as u64);
            assert!(sim.ecg_streaming);
        }
    }

    #[tokio::test]
    async fn ecg_missing_invalid_or_failed_capacity_never_dispatches_an_oversized_frame() {
        for capacity in [
            Ok(None),
            Ok(Some(12)),
            Err("injected writer query failure".to_string()),
        ] {
            let mut sim = SimState::new(SimConfig::default());
            let started = Instant::now() - Duration::from_secs(1);
            sim.ecg_streaming = true;
            sim.ecg_started_at = Some(started);
            let acc = acc::Settings {
                sample_rate_hz: 200,
                range_g: 8,
            };
            sim.acc_settings = Some(acc);
            let mut radio = EcgRadio {
                capacity: capacity.clone(),
                frames: Vec::new(),
            };
            let mut log = EventLog::new();
            let (sender, mut events) = mpsc::channel(16);
            log.add_listener(sender);
            send_ecg(
                &mut radio,
                &mut sim,
                &mut log,
                &mut None,
                started - Duration::from_secs(10),
                sim::POLAR_EPOCH_OFFSET_NS + 5_000_000_000,
            )
            .await;
            assert!(radio.frames.is_empty());
            assert_eq!(sim.ecg_sample_index, 0);
            assert_eq!(sim.delivery_seq, 0);
            assert_eq!(sim.acc_settings, Some(acc), "ECG refusal cannot stop ACC");
            match capacity {
                Ok(None) => {
                    assert!(sim.ecg_streaming);
                    assert!(events.try_recv().is_err());
                }
                Ok(Some(_)) => {
                    assert!(!sim.ecg_streaming);
                    assert_eq!(events.try_recv().unwrap()["kind"], "ecg-stream-failed");
                }
                Err(_) => {
                    assert!(sim.ecg_streaming);
                    let event = events.try_recv().unwrap();
                    assert_eq!(event["op"], "ecg-notification-capacity");
                    assert!(event["error"]
                        .as_str()
                        .unwrap()
                        .contains("injected writer query failure"));
                }
            }
        }
    }

    #[test]
    fn ecg_capacity_geometry_keeps_configured_maximum_and_full_samples() {
        for (configured, capacity, count) in [
            (73, 18, 2),
            (73, 229, 73),
            (73, usize::MAX, 73),
            (167, 510, 166),
            (167, 511, 167),
            (167, 512, 167),
            (1, 512, 1),
        ] {
            let actual = gatt_spec::ecg_frame_samples_for_capacity(configured, capacity).unwrap();
            assert_eq!(actual, count);
            assert!(gatt_spec::encode_ecg_frame(0, &vec![0; actual]).len() <= capacity);
        }
        for capacity in [0, 9, 10, 12] {
            assert!(gatt_spec::ecg_frame_samples_for_capacity(73, capacity).is_err());
        }
        assert!(gatt_spec::ecg_frame_samples_for_capacity(0, 512).is_err());
    }

    #[tokio::test]
    async fn ecg_small_writer_reports_backlog_shedding_and_maximum_batches_still_fit() {
        for (configured, capacity, expected_count, expected_shed) in
            [(73, 13, 130, 130), (167, 511, 167, 0)]
        {
            let mut sim = SimState::new(SimConfig {
                ecg_frame_samples: configured,
                ..SimConfig::default()
            });
            let started = Instant::now();
            sim.ecg_streaming = true;
            sim.ecg_started_at = Some(started);
            let mut radio = EcgRadio {
                capacity: Ok(Some(capacity)),
                frames: Vec::new(),
            };
            let mut log = EventLog::new();
            let (sender, mut events) = mpsc::channel(256);
            log.add_listener(sender);
            send_ecg_at(
                &mut radio,
                &mut sim,
                &mut log,
                &mut None,
                started - Duration::from_secs(10),
                sim::POLAR_EPOCH_OFFSET_NS + 5_000_000_000,
                Duration::from_secs(2),
            )
            .await;
            assert_eq!(
                radio
                    .frames
                    .iter()
                    .map(|frame| (frame.len() - 10) / 3)
                    .sum::<usize>(),
                expected_count
            );
            assert!(radio.frames.iter().all(|frame| frame.len() <= capacity));
            let mut shed = 0;
            while let Ok(event) = events.try_recv() {
                if event["kind"] == "ecg-samples-shed" {
                    shed += event["samples"].as_u64().unwrap();
                }
                assert_ne!(event["kind"], "radio-error");
            }
            assert_eq!(shed, expected_shed);
            assert_eq!(
                sim.ecg_sample_index,
                1300 + expected_count as u64 + expected_shed
            );
            assert!(sim.ecg_streaming);
        }
    }

    struct ProfileRadio {
        advertising: std::collections::VecDeque<Result<bool, radio::RadioError>>,
        starts: usize,
        fail_start: bool,
    }
    #[async_trait::async_trait]
    impl PeripheralRadio for ProfileRadio {
        async fn open(_: tokio::sync::mpsc::Sender<RadioEvent>) -> Result<Self, radio::RadioError> {
            unreachable!()
        }
        async fn is_powered(&mut self) -> Result<bool, radio::RadioError> {
            unreachable!()
        }
        async fn is_advertising(&mut self) -> Result<bool, radio::RadioError> {
            self.advertising.pop_front().expect("unexpected query")
        }
        async fn start_advertising(
            &mut self,
            _: &str,
            _: &[Uuid],
        ) -> Result<(), radio::RadioError> {
            self.starts += 1;
            if self.fail_start {
                Err(radio::RadioError("registration refused".into()))
            } else {
                Ok(())
            }
        }
        async fn stop_advertising(&mut self) -> Result<(), radio::RadioError> {
            Ok(())
        }
        async fn add_service(&mut self, _: &radio::ServiceSpec) -> Result<(), radio::RadioError> {
            unreachable!()
        }
        async fn notification_payload_capacity(
            &self,
            _: Uuid,
        ) -> Result<Option<usize>, radio::RadioError> {
            unreachable!()
        }
        async fn notify(&mut self, _: Uuid, _: Vec<u8>) -> Result<SendOutcome, radio::RadioError> {
            unreachable!()
        }
    }

    #[tokio::test]
    async fn profile_radio_failure_restores_config_replays_and_pending_work() {
        for (queries, fail_start, expected_starts) in [
            (
                vec![Err(radio::RadioError("query refused".into()))],
                false,
                0,
            ),
            (
                vec![
                    Ok(true),
                    Err(radio::RadioError("replacement query refused".into())),
                ],
                false,
                0,
            ),
            (vec![Ok(true), Ok(true)], true, 1),
        ] {
            let mut sim = SimState::new(SimConfig::default());
            sim.ecg_replay = Some(vec![19, -4]);
            sim.hr_replay = Some(sim::HrReplay {
                packets: vec![vec![0, 72]],
            });
            sim.hr_replay_index = 9;
            sim.pending_indications.push(sim::PendingIndication {
                due: Instant::now(),
                response: vec![1, 2],
                action: sim::PmdAction::StartEcg,
            });
            let before = format!("{:?}", sim);
            let mut radio = ProfileRadio {
                advertising: queries.into(),
                starts: 0,
                fail_start,
            };
            let mut log = EventLog::new();
            let (sender, mut events) = tokio::sync::mpsc::channel(16);
            log.add_listener(sender);
            let result = load_profile_into(
                &mut sim,
                &mut radio,
                &mut log,
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/profiles/low-battery-legacy.json"
                ),
            )
            .await;
            assert!(result
                .expect_err("uncertain/rejected radio must not acknowledge profile success")
                .contains("refused"));
            assert_eq!(format!("{:?}", sim), before);
            assert_eq!(radio.starts, expected_starts);
            let mut rollback = false;
            while let Ok(event) = events.try_recv() {
                assert_ne!(event["kind"], "profile-loaded");
                rollback |= event["kind"] == "profile-rolled-back";
            }
            assert!(rollback);
        }
    }

    #[tokio::test]
    async fn profile_success_commits_only_after_known_radio_outcome() {
        for (queries, expected_starts) in [(vec![Ok(false)], 0), (vec![Ok(true), Ok(true)], 1)] {
            let mut sim = SimState::new(SimConfig::default());
            sim.ecg_replay = Some(vec![19]);
            sim.hr_replay = Some(sim::HrReplay {
                packets: vec![vec![0, 72]],
            });
            sim.hr_replay_index = 9;
            sim.pending_indications.push(sim::PendingIndication {
                due: Instant::now(),
                response: vec![1],
                action: sim::PmdAction::StartEcg,
            });
            let mut radio = ProfileRadio {
                advertising: queries.into(),
                starts: 0,
                fail_start: false,
            };
            let mut log = EventLog::new();
            let (sender, mut events) = tokio::sync::mpsc::channel(16);
            log.add_listener(sender);
            let path = concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/profiles/low-battery-legacy.json"
            );
            load_profile_into(&mut sim, &mut radio, &mut log, path)
                .await
                .unwrap();
            assert_eq!(sim.config.profile_path.as_deref(), Some(path));
            assert_eq!(sim.ecg_replay, None);
            assert_eq!(sim.hr_replay, None);
            assert_eq!(sim.hr_replay_index, 0);
            assert!(sim.pending_indications.is_empty());
            assert_eq!(radio.starts, expected_starts);
            let mut kinds = Vec::new();
            while let Ok(event) = events.try_recv() {
                kinds.push(event["kind"].as_str().unwrap().to_owned());
            }
            assert_eq!(kinds.last().map(String::as_str), Some("profile-loaded"));
            assert_eq!(
                kinds
                    .iter()
                    .filter(|kind| *kind == "profile-loaded")
                    .count(),
                1
            );
            assert!(!kinds.iter().any(|kind| kind == "profile-rolled-back"));
        }
    }

    fn comparison_report(passed: bool, complete: bool) -> compare::ComparisonReport {
        compare::ComparisonReport {
            passed,
            complete,
            tolerances: compare::Tolerances::default(),
            fields: Vec::new(),
        }
    }

    #[test]
    fn compare_exit_code_needs_passed_and_complete() {
        // A passed-but-incomplete run (unverified payload bytes, parent
        // paths, adv interval) must not read as qualified.
        assert_eq!(
            compare_exit_code(&comparison_report(true, true), false),
            ExitCode::SUCCESS
        );
        assert_eq!(
            compare_exit_code(&comparison_report(true, false), false),
            ExitCode::from(1)
        );
        assert_eq!(
            compare_exit_code(&comparison_report(true, false), true),
            ExitCode::SUCCESS
        );
        assert_eq!(
            compare_exit_code(&comparison_report(false, false), true),
            ExitCode::from(1)
        );
        assert_eq!(
            compare_exit_code(&comparison_report(false, true), false),
            ExitCode::from(1)
        );
    }

    #[test]
    fn ecg_frame_stamp_is_the_last_sample_not_one_past_it() {
        // The frame holds samples [index, index+count); the strap stamps the
        // last sample, so the first frame (73 samples @130 Hz) stamps sample
        // 72, i.e. 72/130 s after boot — not 73/130 s.
        let mut clock = sample_clock::SampleClock::new(130);
        let first = clock.next(Duration::from_secs(1), 73).unwrap().unwrap();
        let first_ns = sim::device_timestamp_from_elapsed_ns(
            sim::DeviceClock::Unsynchronized,
            0,
            first.last_sample_ns,
        )
        .unwrap();
        assert_eq!(first_ns, 72 * 1_000_000_000 / 130);
    }

    #[test]
    fn queued_stream_logs_correlate_frames_and_preserve_failure_detail() {
        for (op, id, samples) in [("ecg-notify", 41, 73), ("acc-notify", 42, 20)] {
            let frame = json!({"samples": samples, "bytes": 130, "timestampNs": "1234"});
            let (kind, request) =
                stream_notify_log(op, Ok(SendOutcome::Queued { id }), frame.clone()).unwrap();
            assert_eq!(kind, op);
            assert_eq!(request["id"], id);
            assert_eq!(request["outcome"], "queued");
            assert_eq!(request["samples"], samples);
            assert_eq!(request["timestampNs"], "1234");
            for outcome in [
                SendOutcome::OsAccepted,
                SendOutcome::NotSubscribed,
                SendOutcome::Failed("gone".into()),
            ] {
                let (_, settled) = notify_settled_log(id, "svc", gatt_spec::pmd::DATA, &outcome);
                assert_eq!(settled["id"], request["id"]);
            }
            for failure in [
                Ok(SendOutcome::Failed("backpressured".into())),
                Err(radio::RadioError("backend refused".into())),
            ] {
                let (kind, failed) = stream_notify_log(op, failure, frame.clone()).unwrap();
                assert_eq!(kind, "radio-error");
                assert_eq!(failed["op"], op);
                assert_eq!(failed["samples"], samples);
                assert_eq!(failed["timestampNs"], "1234");
                assert!(failed["error"].is_string());
                assert!(failed.get("id").is_none());
            }
        }
        assert!(
            stream_notify_log("acc-notify", Ok(SendOutcome::NotSubscribed), json!({})).is_none()
        );
        let (_, inline) = stream_notify_log(
            "acc-notify",
            Ok(SendOutcome::OsAccepted),
            json!({"samples": 1}),
        )
        .unwrap();
        assert_eq!(inline["outcome"], "os-accepted");
        assert!(inline.get("id").is_none());
    }

    #[test]
    fn every_notify_settle_logs_a_line() {
        // A queued-but-unsettled frame must read as missing its answer, not
        // as delivered: every SendOutcome maps to exactly one log line, and
        // successes speak as `notify-settled` (previously silent) while
        // failures stay loud as `radio-error`.
        for outcome in [
            SendOutcome::OsAccepted,
            SendOutcome::Queued { id: 1 },
            SendOutcome::NotSubscribed,
            SendOutcome::Failed("gone".to_string()),
        ] {
            let (kind, detail) = notify_settled_log(7, "svc", "chr", &outcome);
            assert!(
                kind == "notify-settled" || kind == "radio-error",
                "every settle outcome must log, got {kind} for {outcome:?}"
            );
            assert_eq!(detail["service"], serde_json::json!("svc"));
            assert_eq!(detail["characteristic"], serde_json::json!("chr"));
            assert_eq!(detail["id"], 7);
        }
        let (kind, _) = notify_settled_log(7, "svc", "chr", &SendOutcome::OsAccepted);
        assert_eq!(kind, "notify-settled");
        let (kind, _) = notify_settled_log(7, "svc", "chr", &SendOutcome::Queued { id: 1 });
        assert_eq!(kind, "notify-settled");
    }

    #[test]
    fn drop_summary_with_no_targets_says_no_simulator_clients() {
        let report = radio::DisconnectReport::new();
        let (note, state) = summarize_drop(&[], &[], &report, &[]);
        assert!(
            note.contains("no simulator clients"),
            "empty targets must never read as a silent success: {note}"
        );
        assert_eq!(state["dropped"], serde_json::json!([]));
    }

    #[test]
    fn drop_summary_names_dropped_and_skip_reasons() {
        let mut report = radio::DisconnectReport::new();
        report.add_dropped("AA:AA:AA:AA:AA:AA".to_string());
        report.skip(
            "BB:BB:BB:BB:BB:BB".to_string(),
            radio::SKIP_NOT_CONNECTED.to_string(),
        );
        report.skip(
            "CC:CC:CC:CC:CC:CC".to_string(),
            radio::SKIP_NOT_CLIENT.to_string(),
        );
        let clients = ["AA:AA:AA:AA:AA:AA".to_string()];
        let allowlist = ["BB:BB:BB:BB:BB:BB".to_string()];
        let (note, state) = summarize_drop(&clients, &allowlist, &report, &[]);
        assert!(note.contains("AA:AA:AA:AA:AA:AA"), "{note}");
        assert_eq!(state["dropped"], serde_json::json!(["AA:AA:AA:AA:AA:AA"]));
        assert_eq!(state["skipped"].as_array().unwrap().len(), 2);
    }
}
