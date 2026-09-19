mod advertisement;
mod compare;
mod control;
mod driver;
mod ecg;
mod events;
mod gatt_spec;
mod linux_advertising;
mod mgmt;
mod profile;
mod radio;
mod sim;
mod timing;
mod vectors;

use std::process::ExitCode;
use std::time::{Duration, Instant};

use chrono::Utc;
use radio::{
    adv_service_uuids, h10_services, short_of, PeripheralRadio, PlatformRadio, RadioEvent,
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
         \x20 --name <name>          Advertised name (overrides the profile)\n\
         \x20 --control-bind <addr>  Control port bind address (default 127.0.0.1)\n\
         \x20 --control-port <port>  JSON-lines TCP control port (default 17935)\n\
         \x20 --control-token <tok>  Control port token (or H10SIM_TOKEN / --control-token-file)\n\
         \x20 --control-token-file <path>  Read the control token from a file\n\
         \x20 --pair-policy <policy> Pairing policy: just-works (default) or disabled\n\
         \x20 --bpm <bpm>            Heart rate in bpm (overrides the profile)\n\
         \x20 --battery <percent>    Battery level percent (overrides the profile)\n\
         \x20 --ecg-file <path>      Replay recorded ECG (text, one integer µV per line @130 Hz)\n\
         \x20 --driver <url>        Join the test driver as a peripheral-sim host (ws://host:port/path)\n\
         \x20 --emit-driver-hello    Print the driver hello JSON and exit (no radio)\n\
         \x20 --linux-advertising <mode>  Linux only: bluez (default, LEAdvertisement1 via bluetoothd)\n\
         \x20                        or mgmt-legacy (MGMT_OP_ADD_ADVERTISING; needs CAP_NET_ADMIN, see README)\n\
         \n\
         \x20 A non-loopback --control-bind without a token refuses to start.\n\
         \x20 --emit-test-vectors    Print encoder vectors as JSON and exit (no radio)\n\
         \x20 --timing-profile <path>  Timing profile: an h10-capture fingerprint or timing JSON (default: UNCONFIRMED placeholders)\n\
         \x20 --timing-seed <u64>    Seed for semi-random timing sampling (default 0; same seed replays a run)\n\
         \x20 --emit-timing-defaults  Print the UNCONFIRMED default timing profile as JSON and exit (no radio)\n\
         \x20 --compare <real.json> <sim.json>  Compare fingerprints field by field, print the report and exit (no radio)\n\
         \x20 --tolerance-p50 <f>    Relative p50 tolerance for timing checks (default 0.25)\n\
         \x20 --tolerance-ms <f>     Absolute floor in ms for timing checks (default 50)\n\
         \x20 --help                Print this help\n\
         \n\
         \x20 Later flags win: --profile applies first, then --name/--bpm/--battery.\n")
        .to_string()
}

/// Stock identity, compiled in so the default works from any directory.
const STOCK_PROFILE_JSON: &str = include_str!("../profiles/stock-h10.json");

/// Reads a timing profile: a full `h10-capture` fingerprint or a raw timing
/// profile document. A bad path or bad file is a loud startup failure —
/// never a silent fall back to defaults.
fn load_timing_profile(path: &str, seed: u64) -> Result<timing::TimingProfile, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read timing profile {path}: {error}"))?;
    timing::TimingProfile::from_fingerprint_json(&text, seed)
}

/// Compares a real-strap fingerprint against a simulator fingerprint and
/// prints the field-by-field report as JSON. Exits 0 when every check
/// passed, 1 otherwise.
fn run_compare(real_path: &str, sim_path: &str, tolerances: compare::Tolerances) -> ExitCode {
    let read = |path: &str| {
        std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read {path}: {error}"))
            .and_then(|text| {
                serde_json::from_str(&text).map_err(|error| format!("cannot parse {path}: {error}"))
            })
    };
    let report = (|| -> Result<compare::ComparisonReport, String> {
        let real: serde_json::Value = read(real_path)?;
        let sim: serde_json::Value = read(sim_path)?;
        Ok(compare::compare_fingerprints(&real, &sim, tolerances))
    })();
    match report {
        Ok(report) => {
            match serde_json::to_string_pretty(&report) {
                Ok(json) => println!("{json}"),
                Err(error) => {
                    eprintln!("h10-sim: cannot encode comparison report: {error}");
                    return ExitCode::FAILURE;
                }
            }
            if report.passed {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(message) => {
            eprintln!("h10-sim: {message}");
            ExitCode::from(2)
        }
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
    let mut timing_profile_path: Option<String> = None;
    let mut timing_seed: u64 = 0;
    let mut compare_paths: Option<(String, String)> = None;
    let mut tolerance_p50 = compare::Tolerances::default().p50_relative;
    let mut tolerance_ms = compare::Tolerances::default().min_abs_ms;
    let mut linux_advertising = LinuxAdvertising::default();

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
                "--name" => config.name = value(&mut args, "--name")?,
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
                "--ecg-file" => {
                    let file = value(&mut args, "--ecg-file")?;
                    config.ecg_source = sim::EcgSource::File { file };
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
    if let Some((real, sim)) = compare_paths {
        return run_compare(
            &real,
            &sim,
            compare::Tolerances {
                p50_relative: tolerance_p50,
                min_abs_ms: tolerance_ms,
            },
        );
    }
    // Timing placeholders stay until a capture confirms them; a bad profile
    // path is a loud startup failure.
    let timing_profile = match timing_profile_path {
        Some(path) => match load_timing_profile(&path, timing_seed) {
            Ok(profile) => profile,
            Err(message) => {
                eprintln!("h10-sim: {message}");
                return ExitCode::from(2);
            }
        },
        None => timing::TimingProfile::default_unconfirmed(timing_seed),
    };
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
        )
    }
}

/// Loads a profile file onto the startup config. A bad path or bad file is a
/// loud startup failure — never a silent fall back to defaults.
fn load_startup_profile(config: &mut SimConfig, path: &str) -> Result<(), String> {
    let profile = profile::load_profile(path)?;
    config.apply_profile(path, &profile)
}

fn run(
    config: SimConfig,
    control_bind: String,
    control_port: u16,
    control_token: Option<String>,
    driver_url: Option<String>,
    timing_profile: timing::TimingProfile,
    linux_advertising: LinuxAdvertising,
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
/// A missing or malformed file is a loud startup/profile-load failure.
fn load_ecg_replay(source: &sim::EcgSource) -> Result<Option<Vec<i32>>, String> {
    match source {
        sim::EcgSource::Synthetic => Ok(None),
        sim::EcgSource::File { file } => ecg::load_replay_file(file).map(Some),
    }
}

async fn serve(
    config: SimConfig,
    control_bind: String,
    control_port: u16,
    control_token: Option<String>,
    driver_url: Option<String>,
    timing_profile: timing::TimingProfile,
    linux_advertising: LinuxAdvertising,
) -> Result<(), Fatal> {
    let mut log = EventLog::new();
    let mut sim = SimState::new(config);
    sim.ecg_replay = load_ecg_replay(&sim.config.ecg_source)?;
    let mut timing = timing::TimingRuntime::new(timing_profile);
    log.log(
        "timing-profile",
        json!({"seed": timing.profile.seed, "confirmed": timing.profile.fully_confirmed(), "unconfirmed": timing.profile.unconfirmed_fields()}),
    );
    let boot_epoch_ns: u64 = Utc::now().timestamp_nanos_opt().unwrap_or(0).max(0) as u64;

    let (radio_tx, mut radio_rx) = mpsc::channel::<RadioEvent>(256);
    let mut radio = PlatformRadio::open(radio_tx)
        .await
        .map_err(|error| error.to_string())?;

    wait_powered(&mut radio, &mut log).await?;
    select_linux_advertising(&mut radio, linux_advertising, &mut log)?;

    for service in h10_services(&sim.config).map_err(|error| error.to_string())? {
        radio
            .add_service(&service)
            .await
            .map_err(|error| error.to_string())?;
    }
    log.log(
        "services-registered",
        json!({"services": ["180D", "180A", "180F", "6217ff4b", "PMD", "FEEE"]}),
    );

    if let Err(message) = start_advertising(&mut radio, &sim, &mut log).await {
        return Err(if radio.advertising_unavailable() {
            Fatal::unavailable(message)
        } else {
            Fatal::from(message)
        });
    }

    let (control_tx, mut control_rx) = mpsc::channel::<ControlRequest>(64);
    let control_server = tokio::spawn(control::serve(
        control_bind.clone(),
        control_port,
        control_token.clone(),
        control_tx.clone(),
    ));
    log.log(
        "control-listening",
        json!({"bind": control_bind, "port": control_port, "auth": control_token.is_some()}),
    );

    if let Some(url) = driver_url {
        let (event_tx, event_rx) = mpsc::channel::<serde_json::Value>(256);
        log.add_listener(event_tx);
        log.log("driver-joining", json!({"url": url}));
        let driver_commands = control_tx.clone();
        let host_label = format!("peripheral-sim/{}", std::env::consts::OS);
        tokio::spawn(async move {
            match driver::run(url, host_label, driver_commands, event_rx).await {
                Ok(()) => eprintln!("h10-sim: driver connection closed"),
                Err(error) => eprintln!("h10-sim: driver exited: {error}"),
            }
        });
    }

    let mut next_hr = Instant::now();
    let mut next_ecg = Instant::now();
    let mut next_battery = Instant::now() + Duration::from_secs(60);
    let mut last_tick = Instant::now();
    let boot = Instant::now();
    let tick = tokio::time::interval(Duration::from_millis(50));
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
                    next_ecg = now + Duration::from_secs_f64(timing.ecg_interval_s(sim.config.ecg_frames_per_sec));
                    send_ecg(&mut radio, &mut sim, &mut log, boot_epoch_ns).await;
                }
                if now >= next_battery {
                    next_battery = now + Duration::from_secs(60);
                    send_battery(&mut radio, &sim, &mut log).await;
                }
            }
            Some(event) = radio_rx.recv() => {
                handle_radio(event, &mut radio, &mut sim, &mut timing, &mut log, boot_epoch_ns).await;
            }
            Some(request) = control_rx.recv() => {
                handle_control(request, &mut radio, &mut sim, &mut log).await;
            }
        }
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
    radio: &mut PlatformRadio,
    sim: &SimState,
    log: &mut EventLog,
) -> Result<(), String> {
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
    match radio.notify(uuid, payload.clone()).await {
        // With no subscriber there is no delivery to report; a dead session
        // is loud below.
        Ok(true) => log.log("hr-notify", json!({"bpm": sim.config.bpm})),
        Ok(false) => {}
        Err(error) => log.log(
            "radio-error",
            json!({"op": "hr-notify", "error": error.to_string()}),
        ),
    }
}

async fn send_battery(radio: &mut PlatformRadio, sim: &SimState, log: &mut EventLog) {
    if sim.silent {
        return;
    }
    let payload = gatt_spec::encode_battery_level(sim.config.battery_percent);
    let uuid = advertisement::short_uuid(gatt_spec::uuid16::BATTERY_LEVEL);
    match radio.notify(uuid, payload).await {
        Ok(true) => log.log(
            "battery-notify",
            json!({"percent": sim.config.battery_percent}),
        ),
        Ok(false) => {}
        Err(error) => {
            log.log(
                "radio-error",
                json!({"op": "battery-notify", "error": error.to_string()}),
            );
        }
    }
}

async fn send_ecg(
    radio: &mut PlatformRadio,
    sim: &mut SimState,
    log: &mut EventLog,
    boot_epoch_ns: u64,
) {
    let count = sim.config.ecg_frame_samples;
    let index = sim.ecg_sample_index;
    sim.ecg_sample_index = index.saturating_add(count as u64);
    if !sim.ecg_streaming || sim.silent {
        return;
    }
    let mut samples = Vec::with_capacity(count);
    match &sim.ecg_replay {
        Some(recorded) => ecg::replay_samples(recorded, index, count, &mut samples),
        None => ecg::ecg_frame_samples(index, count, f64::from(sim.config.bpm), &mut samples),
    }
    let timestamp_ns = boot_epoch_ns.saturating_add(
        index
            .saturating_add(count as u64)
            .saturating_mul(1_000_000_000)
            / 130,
    );
    let frame = gatt_spec::encode_ecg_frame(timestamp_ns, &samples);
    let uuid = Uuid::parse_str(gatt_spec::pmd::DATA).unwrap_or_else(|_| Uuid::nil());
    match radio.notify(uuid, frame.clone()).await {
        Ok(true) => log.log(
            "ecg-notify",
            json!({"samples": samples.len(), "bytes": frame.len(), "timestampNs": timestamp_ns.to_string()}),
        ),
        Ok(false) => {}
        Err(error) => {
            log.log("radio-error", json!({"op": "ecg-notify", "error": error.to_string()}));
        }
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
    _boot_epoch_ns: u64,
) {
    match event {
        RadioEvent::Powered(powered) => {
            log.log("powered", json!({"on": powered}));
        }
        RadioEvent::Subscription {
            service,
            characteristic,
            subscribed,
        } => {
            log.log(
                if subscribed {
                    "subscribed"
                } else {
                    "unsubscribed"
                },
                json!({"service": service, "characteristic": characteristic}),
            );
        }
        RadioEvent::IndicationConfirmed {
            service,
            characteristic,
        } => {
            log.log(
                "indication-confirmed",
                json!({"service": service, "characteristic": characteristic}),
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
        RadioEvent::Write {
            service,
            characteristic,
            value,
            reply,
        } => {
            let uuid = Uuid::parse_str(&characteristic).unwrap_or_else(|_| Uuid::nil());
            let accepted = write_request(radio, sim, timing, log, &service, &uuid, &value).await;
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
        // Writable vendor characteristics (6217ff4d, FEEE 0x51/0x53) have no
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
    let outcome = sim.handle_pmd_write(value);
    match outcome.indicate {
        Some(response) => {
            let status = response.get(3).copied().unwrap_or(0xFF);
            // The measured PMD response latency, once a capture confirmed
            // it; immediate by default so behaviour is unchanged.
            timing.pmd_response_delay().await;
            // Awaited: a lost indication surfaces here instead of timing out
            // the central 5 s later.
            match radio.notify(control_point, response.clone()).await {
                Ok(true) => {}
                Ok(false) => {
                    log.log(
                        "radio-error",
                        json!({"op": "pmd-indicate", "error": "no live indication session"}),
                    );
                    return false;
                }
                Err(error) => {
                    log.log(
                        "radio-error",
                        json!({"op": "pmd-indicate", "error": error.to_string()}),
                    );
                    return false;
                }
            }
            log.log(
                "pmd-command",
                json!({"op": value.first().copied().unwrap_or(0xFF), "status": status, "action": format!("{:?}", outcome.action)}),
            );
            match outcome.action {
                PmdAction::StartEcg => log.log_simple("ecg-started"),
                PmdAction::StopEcg => log.log_simple("ecg-stopped"),
                PmdAction::None => {}
            }
            true
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
        ControlCommand::SetSilent { on } => {
            sim.silent = on;
            log.log("silent", json!({"on": on}));
            ControlReply::ok()
        }
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
            log.log("pmd-fault-armed", json!({"status": status}));
            ControlReply::ok()
        }
        ControlCommand::ClearPmdFault => {
            sim.reject_next_status = None;
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

/// Loads a profile file onto the live sim. A bad path keeps the old profile
/// and answers `{"ok":false}` — never a silent partial swap. When the radio
/// is advertising, the advertisement is re-registered so the new name takes
/// effect over the air.
async fn load_profile_into(
    sim: &mut SimState,
    radio: &mut PlatformRadio,
    log: &mut EventLog,
    path: &str,
) -> Result<(), String> {
    let loaded = profile::load_profile(path)?;
    // Resolve the replay file before swapping the config: a bad file keeps
    // the old profile instead of a half-applied one.
    let replay = load_ecg_replay(&loaded.pmd.ecg_source)?;
    sim.config.apply_profile(path, &loaded)?;
    sim.ecg_replay = replay;
    log.log("profile-loaded", json!({"path": path}));
    match radio.is_advertising().await {
        Ok(true) => {
            start_advertising(radio, sim, log).await?;
        }
        Ok(false) => {}
        Err(error) => {
            log.log(
                "radio-error",
                json!({"op": "is-advertising", "error": error.to_string()}),
            );
        }
    }
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

async fn drop_link(
    radio: &mut PlatformRadio,
    sim: &mut SimState,
    log: &mut EventLog,
) -> Result<ControlReply, String> {
    // A simulated link loss drops the link, not the peripheral: advertising
    // and the GATT database stay exactly as they were, so centrals see a
    // lifecycle loss (peer-link-loss) with no Service Changed, and can
    // reconnect immediately — like walking back into range of a real H10.
    sim.ecg_streaming = false;
    // Genuine disconnect where the backend has the API: BlueZ drops every
    // connected central via Device1.Disconnect (counted below). CoreBluetooth
    // has no force-disconnect API, so a connected central stays up there.
    let dropped = radio
        .disconnect_centrals()
        .await
        .map_err(|error| error.to_string())?;
    log.log("link-dropped", json!({"disconnected": dropped}));
    Ok(ControlReply::ok_note(format!(
        "ECG halted, {dropped} central(s) disconnected; advertising and the \
         GATT database are unchanged (BlueZ disconnects via \
         Device1.Disconnect; on CoreBluetooth an already-connected central \
         stays connected until it disconnects)"
    )))
}
