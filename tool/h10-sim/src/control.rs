//! JSON-lines TCP control port: live configuration and fault injection.
//!
//! One JSON object per line on stdin of the socket; one JSON reply per line.
//! Unknown commands and invalid arguments are answered with
//! `{"ok":false,"error":"…"}` and logged — never silently ignored.

use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
};

use crate::sim::SimConfig;

/// A request the control server forwards to the simulator loop.
#[derive(Debug)]
pub struct ControlRequest {
    pub command: ControlCommand,
    pub reply: oneshot::Sender<ControlReply>,
}

/// Run posture: `faithful` reproduces the captured H10 behaviour and
/// injects nothing; `adversarial` additionally allows labelled fault
/// injection through explicit control commands. Selected once at startup
/// with `--mode` (default `faithful`); profiles cannot change it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunMode {
    #[default]
    Faithful,
    Adversarial,
}

impl RunMode {
    /// Parses `--mode`: exactly `faithful` or `adversarial`, fail-closed.
    pub fn parse(text: &str) -> Result<Self, String> {
        match text {
            "faithful" => Ok(Self::Faithful),
            "adversarial" => Ok(Self::Adversarial),
            _ => Err(format!(
                "--mode {text:?} is not a run mode (want faithful or adversarial)"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Faithful => "faithful",
            Self::Adversarial => "adversarial",
        }
    }
}

/// Commands accepted on the control port.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum ControlCommand {
    /// Set the heart rate: `{"cmd":"set-bpm","bpm":96}`.
    SetBpm { bpm: u8 },
    /// Start or stop advertising: `{"cmd":"set-advertising","on":false}`.
    SetAdvertising { on: bool },
    /// Halt ECG and disconnect the simulator's own clients — centrals whose
    /// addresses touched this peripheral's GATT application — plus any
    /// `--drop-link-allow` extras; advertising and the GATT database stay
    /// up, so centrals see a link loss with no Service Changed and can
    /// reconnect immediately. Adversarial: abrupt disconnect.
    DropLink,
    /// Stop notifying while keeping the link up (link-loss `silent` fault).
    /// Adversarial.
    SetSilent { on: bool },
    /// Fail the next PMD command with a status code, e.g. `{"cmd":"reject-next-pmd","status":3}`.
    /// Adversarial.
    RejectNextPmd { status: u8 },
    /// Clear a pending PMD fault without firing it. Adversarial (fault
    /// workflow control).
    ClearPmdFault,
    /// Delay PMD responses by an extra `ms` milliseconds on top of any
    /// measured latency, e.g. `{"cmd":"delay-responses","ms":250}` (`0`
    /// clears). Adversarial: delayed responses.
    DelayResponses { ms: u64 },
    /// Drop simulator-client links and bounce advertising so centrals run a
    /// rapid disconnect/reconnect cycle. Adversarial: rapid reconnect.
    FlapLink,
    /// Tear down the next notify/indicate subscription as soon as it is set
    /// up. Adversarial: interrupted subscription setup.
    InterruptNextSubscribe,
    /// Re-notify the last PMD response out of sequence (fails loudly when no
    /// PMD response has gone out yet). Adversarial: stale callback.
    StaleCallback,
    /// Shed ECG frames, delivering every `keepEvery`-th frame only, e.g.
    /// `{"cmd":"constrain-delivery","keepEvery":4}` (`1` disables).
    /// Adversarial: constrained delivery capacity.
    ConstrainDelivery {
        #[serde(rename = "keepEvery")]
        keep_every: u64,
    },
    /// Report this run's seed/profile, mode and injected fault sequence with
    /// timestamps. Telemetry, available in every mode.
    RunRecord,
    /// Set the battery level now: `{"cmd":"set-battery","level":15}` (0–100).
    SetBattery { level: u8 },
    /// Report sensor contact lost or detected: `{"cmd":"set-contact","detected":false}`.
    /// Takes effect over the air only when the profile declares contact
    /// supported (the strap reports contact not supported, so the stock
    /// profile records the state without changing the HR flags).
    SetContact { detected: bool },
    /// Switch the pairing policy: `{"cmd":"pair-policy","policy":"disabled"}`.
    PairPolicy { policy: crate::sim::PairPolicy },
    /// Load a profile file live: `{"cmd":"load-profile","path":"profiles/stock-h10.json"}`.
    LoadProfile { path: String },
    /// Change stream rates: `{"cmd":"set-rates","hrHz":2.0,"ecgFramesPerSec":4.0}`.
    /// ECG frames carry `ecgFrameSamples` samples (1..=167 so a frame fits MTU 512).
    SetRates {
        #[serde(rename = "hrHz")]
        hr_hz: Option<f64>,
        #[serde(rename = "ecgFramesPerSec")]
        ecg_frames_per_sec: Option<f64>,
        #[serde(rename = "ecgFrameSamples")]
        ecg_frame_samples: Option<usize>,
    },
    /// Report the current simulator state.
    GetState,
    /// List the commands.
    Help,
}

impl ControlCommand {
    /// Kebab-case name, mirroring [`COMMANDS`]: the single source the
    /// driver echo, refusal messages and fault records share.
    pub fn name(&self) -> &'static str {
        match self {
            Self::SetBpm { .. } => "set-bpm",
            Self::SetBattery { .. } => "set-battery",
            Self::SetContact { .. } => "set-contact",
            Self::PairPolicy { .. } => "pair-policy",
            Self::LoadProfile { .. } => "load-profile",
            Self::SetAdvertising { .. } => "set-advertising",
            Self::DropLink => "drop-link",
            Self::SetSilent { .. } => "set-silent",
            Self::RejectNextPmd { .. } => "reject-next-pmd",
            Self::ClearPmdFault => "clear-pmd-fault",
            Self::DelayResponses { .. } => "delay-responses",
            Self::FlapLink => "flap-link",
            Self::InterruptNextSubscribe => "interrupt-next-subscribe",
            Self::StaleCallback => "stale-callback",
            Self::ConstrainDelivery { .. } => "constrain-delivery",
            Self::SetRates { .. } => "set-rates",
            Self::RunRecord => "run-record",
            Self::GetState => "get-state",
            Self::Help => "help",
        }
    }

    /// Whether the command injects a labelled fault: only meaningful in
    /// `adversarial` mode. Configuration, telemetry and help stay available
    /// in `faithful` mode. Exhaustive on purpose: a new variant cannot
    /// compile without a faithful/adversarial decision here.
    pub fn is_adversarial(&self) -> bool {
        match self {
            Self::DropLink
            | Self::SetSilent { .. }
            | Self::RejectNextPmd { .. }
            | Self::ClearPmdFault
            | Self::DelayResponses { .. }
            | Self::FlapLink
            | Self::InterruptNextSubscribe
            | Self::StaleCallback
            | Self::ConstrainDelivery { .. } => true,
            Self::SetBpm { .. }
            | Self::SetAdvertising { .. }
            | Self::SetBattery { .. }
            | Self::SetContact { .. }
            | Self::PairPolicy { .. }
            | Self::LoadProfile { .. }
            | Self::SetRates { .. }
            | Self::RunRecord
            | Self::GetState
            | Self::Help => false,
        }
    }
}

/// Refuses an adversarial fault command outside adversarial mode, loudly
/// naming the command and the mode. Everything else passes in every mode.
pub fn check_mode(mode: RunMode, command: &ControlCommand) -> Result<(), String> {
    if mode == RunMode::Faithful && command.is_adversarial() {
        return Err(format!(
            "{} is an adversarial fault command: refused in faithful mode \
             (restart with --mode adversarial to inject faults)",
            command.name()
        ));
    }
    Ok(())
}

/// Every command the control port (and the driver `sim-control` scenario)
/// accepts: kebab-case name plus one-line description. This table is the
/// single source of truth for `help` replies and the driver hello — a new
/// command is added here, never in two places.
pub const COMMANDS: &[(&str, &str)] = &[
    ("set-bpm", "set heart rate in bpm"),
    ("set-battery", "set battery level 0-100"),
    ("set-contact", "set sensor-contact detected/lost"),
    ("pair-policy", "set pairing policy just-works/disabled"),
    ("load-profile", "load a JSON device profile live"),
    ("set-advertising", "start/stop advertising"),
    (
        "drop-link",
        "halt ECG and disconnect simulator clients (plus --drop-link-allow extras); advertising and GATT stay up [adversarial]",
    ),
    ("set-silent", "stop notifying while keeping the link up [adversarial]"),
    (
        "reject-next-pmd",
        "fail the next PMD command with a status code [adversarial]",
    ),
    (
        "clear-pmd-fault",
        "disarm a pending PMD fault [adversarial]",
    ),
    (
        "delay-responses",
        "add ms of extra PMD response latency, 0 clears [adversarial]",
    ),
    (
        "flap-link",
        "drop client links and bounce advertising for a rapid reconnect [adversarial]",
    ),
    (
        "interrupt-next-subscribe",
        "tear down the next subscription as soon as it is set up [adversarial]",
    ),
    (
        "stale-callback",
        "re-notify the last PMD response out of sequence [adversarial]",
    ),
    (
        "constrain-delivery",
        "deliver every keepEvery-th ECG frame only, 1 disables [adversarial]",
    ),
    ("set-rates", "change HR/ECG stream rates"),
    ("run-record", "report seed/profile, mode and injected fault sequence"),
    ("get-state", "report the current simulator state"),
    ("help", "list the commands"),
];

/// Pipe-joined command names for `help` replies.
pub fn command_help() -> String {
    COMMANDS
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Reply sent back over the control port.
#[derive(Debug, Clone, Serialize)]
pub struct ControlReply {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<serde_json::Value>,
}

impl ControlReply {
    pub fn ok() -> Self {
        Self {
            ok: true,
            error: None,
            note: None,
            state: None,
        }
    }

    pub fn ok_note(note: impl Into<String>) -> Self {
        Self {
            ok: true,
            error: None,
            note: Some(note.into()),
            state: Some(serde_json::Value::Null),
        }
    }

    pub fn failed(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(error.into()),
            note: None,
            state: None,
        }
    }
}

/// Validates a rate-change request against the simulator config. Every field
/// is validated before any field is written: a rejected request leaves the
/// config exactly as it was, never half-applied.
pub fn apply_rates(
    config: &mut SimConfig,
    hr_hz: Option<f64>,
    ecg_frames_per_sec: Option<f64>,
    ecg_frame_samples: Option<usize>,
) -> Result<(), String> {
    if let Some(rate) = hr_hz {
        if !(0.1..=10.0).contains(&rate) {
            return Err(format!("hrHz {rate} is out of range 0.1..=10.0"));
        }
    }
    if let Some(rate) = ecg_frames_per_sec {
        if !(0.5..=10.0).contains(&rate) {
            return Err(format!("ecgFramesPerSec {rate} is out of range 0.5..=10.0"));
        }
    }
    if let Some(samples) = ecg_frame_samples {
        if !(1..=167).contains(&samples) {
            return Err(format!("ecgFrameSamples {samples} is out of range 1..=167"));
        }
    }
    if let Some(rate) = hr_hz {
        config.hr_hz = rate;
    }
    if let Some(rate) = ecg_frames_per_sec {
        config.ecg_frames_per_sec = rate;
    }
    if let Some(samples) = ecg_frame_samples {
        config.ecg_frame_samples = samples;
    }
    Ok(())
}

/// Refuses a non-loopback bind without a token. The token value itself never
/// appears in the error: only the rule.
pub fn check_bind(bind: &str, token: Option<&str>) -> Result<(), String> {
    let loopback = bind == "localhost"
        || bind
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false);
    if loopback {
        return Ok(());
    }
    match token {
        Some(value) if !value.is_empty() => Ok(()),
        _ => Err(format!(
            "control bind {bind} is not a loopback address: refusing to listen \
             without --control-token (or H10SIM_TOKEN / --control-token-file)"
        )),
    }
}

/// Longest accepted control line in bytes, including the newline. Control
/// commands are small JSON objects; anything past this is a framing fault,
/// failed loudly on its own reply — never a growing buffer.
pub const MAX_CONTROL_LINE_BYTES: usize = 8192;

/// How long a connection may take to present its token line before the gate
/// rejects it. Long-lived admitted sessions are unaffected.
pub const CONTROL_AUTH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Binds the control port without accepting: the caller announces
/// `control-listening` only after this succeeds, so readiness is never
/// declared before the bind. Refuses a non-loopback bind without a token
/// before touching the socket.
pub async fn listen(bind: &str, port: u16, token: Option<&str>) -> Result<TcpListener, String> {
    check_bind(bind, token)?;
    TcpListener::bind((bind.to_string(), port))
        .await
        .map_err(|error| format!("cannot bind control port {bind}:{port}: {error}"))
}

/// Accepts connections on an already-bound listener until it errors fatally.
/// Every connection is handled independently; a malformed line fails only its
/// own reply. When `token` is set, each connection's first line must equal it
/// (never logged).
pub async fn serve_listener(
    listener: TcpListener,
    token: Option<String>,
    requests: mpsc::Sender<ControlRequest>,
) -> Result<(), String> {
    loop {
        let (socket, _) = listener
            .accept()
            .await
            .map_err(|error| format!("control accept failed: {error}"))?;
        let requests = requests.clone();
        let token = token.clone();
        tokio::spawn(async move {
            serve_connection(socket, &requests, token).await;
        });
    }
}

async fn serve_connection(
    socket: TcpStream,
    requests: &mpsc::Sender<ControlRequest>,
    expected_token: Option<String>,
) {
    let peer = socket
        .peer_addr()
        .map(|addr| addr.to_string())
        .unwrap_or_else(|_| "?".to_string());
    let (reader, writer) = socket.into_split();
    let admitted = serve_lines_with_auth(
        BufReader::new(reader),
        writer,
        requests.clone(),
        expected_token.as_deref(),
    )
    .await;
    if !admitted {
        // The presented value is never logged — only the peer and the fact.
        eprintln!("h10-sim: control-auth-failed peer={peer}");
    }
}

/// One JSON-lines session over any byte stream, with a first-line token gate
/// (`None` = no gate, today's loopback behaviour). Split out so tests can
/// drive it over an in-memory duplex where listening sockets are unavailable.
/// Returns false when the gate rejected the connection: the caller logs the
/// peer (never the presented value) and closes. A wrong token fails its own
/// reply and forwards nothing.
/// Reads one `\n`-terminated line bounded by [`MAX_CONTROL_LINE_BYTES`].
/// Returns `Ok(None)` on clean EOF with no pending bytes, `Err` on I/O
/// failure or when the line outgrows the bound — the bound is enforced while
/// reading (via `fill_buf`/`consume`), so a peer streaming without newlines
/// cannot grow memory past the cap plus one buffer fill.
async fn read_bounded_line(
    reader: &mut (impl AsyncBufRead + Unpin),
    buf: &mut Vec<u8>,
) -> Result<Option<String>, String> {
    buf.clear();
    loop {
        let chunk = reader
            .fill_buf()
            .await
            .map_err(|error| format!("control read failed: {error}"))?;
        if chunk.is_empty() {
            if buf.is_empty() {
                return Ok(None);
            }
            return Err("control connection closed mid-line".to_string());
        }
        let newline = chunk.iter().position(|byte| *byte == b'\n');
        let take = newline.map(|index| index + 1).unwrap_or(chunk.len());
        if buf.len() + take > MAX_CONTROL_LINE_BYTES {
            return Err(format!(
                "control line exceeds {MAX_CONTROL_LINE_BYTES} bytes"
            ));
        }
        buf.extend_from_slice(&chunk[..take]);
        reader.consume(take);
        if newline.is_some() {
            let line = String::from_utf8_lossy(buf).into_owned();
            return Ok(Some(line));
        }
    }
}

async fn serve_lines_with_auth(
    reader: impl AsyncRead + Unpin,
    mut writer: impl AsyncWrite + Unpin,
    requests: mpsc::Sender<ControlRequest>,
    expected_token: Option<&str>,
) -> bool {
    let mut buffered = BufReader::new(reader);
    let mut buf = Vec::new();
    if let Some(expected) = expected_token {
        let first = tokio::time::timeout(
            CONTROL_AUTH_TIMEOUT,
            read_bounded_line(&mut buffered, &mut buf),
        )
        .await;
        let admitted = match first {
            Ok(Ok(Some(line))) => line.trim_end_matches(['\r', '\n']) == expected,
            _ => false,
        };
        if !admitted {
            let _ = writer
                .write_all(b"{\"ok\":false,\"error\":\"auth failed\"}\n")
                .await;
            return false;
        }
    }
    loop {
        let line = match read_bounded_line(&mut buffered, &mut buf).await {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(error) => {
                let mut encoded = serde_json::to_string(&ControlReply::failed(error))
                    .unwrap_or_else(|_| "{\"ok\":false}".to_string());
                encoded.push('\n');
                let _ = writer.write_all(encoded.as_bytes()).await;
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<ControlCommand>(&line) {
            Ok(command) => {
                let (reply_tx, reply_rx) = oneshot::channel::<ControlReply>();
                if requests
                    .send(ControlRequest {
                        command,
                        reply: reply_tx,
                    })
                    .await
                    .is_err()
                {
                    ControlReply::failed("simulator loop is gone")
                } else {
                    reply_rx
                        .await
                        .unwrap_or_else(|_| ControlReply::failed("simulator loop is gone"))
                }
            }
            Err(error) => ControlReply::failed(format!("invalid control command: {error}")),
        };
        let mut encoded =
            serde_json::to_string(&reply).unwrap_or_else(|_| "{\"ok\":false}".to_string());
        encoded.push('\n');
        if writer.write_all(encoded.as_bytes()).await.is_err() {
            break;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_commands_parse_from_json_lines() {
        let command: ControlCommand =
            serde_json::from_str(r#"{"cmd":"set-bpm","bpm":96}"#).unwrap();
        assert!(matches!(command, ControlCommand::SetBpm { bpm: 96 }));
        let command: ControlCommand = serde_json::from_str(r#"{"cmd":"drop-link"}"#).unwrap();
        assert!(matches!(command, ControlCommand::DropLink));
        let command: ControlCommand =
            serde_json::from_str(r#"{"cmd":"set-rates","hrHz":2.0}"#).unwrap();
        assert!(matches!(command, ControlCommand::SetRates { .. }));
    }

    #[test]
    fn runtime_profile_commands_parse_from_json_lines() {
        let command: ControlCommand =
            serde_json::from_str(r#"{"cmd":"set-battery","level":15}"#).unwrap();
        assert!(matches!(command, ControlCommand::SetBattery { level: 15 }));
        let command: ControlCommand =
            serde_json::from_str(r#"{"cmd":"set-contact","detected":false}"#).unwrap();
        assert!(matches!(
            command,
            ControlCommand::SetContact { detected: false }
        ));
        let command: ControlCommand =
            serde_json::from_str(r#"{"cmd":"pair-policy","policy":"disabled"}"#).unwrap();
        assert!(matches!(command, ControlCommand::PairPolicy { .. }));
        let command: ControlCommand =
            serde_json::from_str(r#"{"cmd":"load-profile","path":"profiles/stock-h10.json"}"#)
                .unwrap();
        assert!(matches!(command, ControlCommand::LoadProfile { .. }));
    }

    #[test]
    fn run_modes_parse_strictly() {
        assert_eq!(RunMode::parse("faithful"), Ok(RunMode::Faithful));
        assert_eq!(RunMode::parse("adversarial"), Ok(RunMode::Adversarial));
        assert!(RunMode::parse("Faithful").is_err());
        assert!(RunMode::parse("").is_err());
        assert!(RunMode::parse("chaos").is_err());
        assert_eq!(RunMode::default(), RunMode::Faithful);
        assert_eq!(RunMode::Faithful.as_str(), "faithful");
        assert_eq!(RunMode::Adversarial.as_str(), "adversarial");
    }

    #[test]
    fn fault_injection_is_adversarial_and_config_is_not() {
        let adversarial = [
            "{\"cmd\":\"drop-link\"}",
            "{\"cmd\":\"set-silent\",\"on\":true}",
            "{\"cmd\":\"reject-next-pmd\",\"status\":3}",
            "{\"cmd\":\"clear-pmd-fault\"}",
            "{\"cmd\":\"delay-responses\",\"ms\":250}",
            "{\"cmd\":\"flap-link\"}",
            "{\"cmd\":\"interrupt-next-subscribe\"}",
            "{\"cmd\":\"stale-callback\"}",
            "{\"cmd\":\"constrain-delivery\",\"keepEvery\":4}",
        ];
        for line in adversarial {
            let command: ControlCommand = serde_json::from_str(line).unwrap();
            assert!(command.is_adversarial(), "{line} must be adversarial-only");
        }
        let open = [
            "{\"cmd\":\"set-bpm\",\"bpm\":90}",
            "{\"cmd\":\"set-rates\",\"hrHz\":2.0}",
            "{\"cmd\":\"get-state\"}",
            "{\"cmd\":\"run-record\"}",
            "{\"cmd\":\"help\"}",
        ];
        for line in open {
            let command: ControlCommand = serde_json::from_str(line).unwrap();
            assert!(
                !command.is_adversarial(),
                "{line} must stay available in faithful mode"
            );
        }
    }

    #[test]
    fn faithful_mode_refuses_adversarial_commands_loudly() {
        let error = check_mode(RunMode::Faithful, &ControlCommand::DropLink)
            .expect_err("drop-link must be refused in faithful mode");
        assert!(
            error.contains("drop-link") && error.contains("faithful"),
            "refusal must name the command and the mode: {error}"
        );
        assert!(check_mode(RunMode::Faithful, &ControlCommand::GetState).is_ok());
        assert!(check_mode(RunMode::Faithful, &ControlCommand::RunRecord).is_ok());
        assert!(check_mode(RunMode::Adversarial, &ControlCommand::DropLink).is_ok());
    }

    #[test]
    fn help_covers_every_command_once() {
        let help = command_help();
        for (name, _) in COMMANDS {
            assert_eq!(
                help.split(" | ").filter(|entry| entry == name).count(),
                1,
                "{name} must appear exactly once in help"
            );
        }
    }

    #[test]
    fn unknown_commands_are_rejected_at_parse_time() {
        assert!(serde_json::from_str::<ControlCommand>(r#"{"cmd":"self-destruct"}"#).is_err());
        assert!(serde_json::from_str::<ControlCommand>(r#"not json"#).is_err());
    }

    #[test]
    fn non_loopback_bind_without_token_is_refused() {
        assert!(check_bind("127.0.0.1", None).is_ok());
        assert!(check_bind("::1", None).is_ok());
        assert!(check_bind("localhost", None).is_ok());
        assert!(check_bind("0.0.0.0", None).is_err());
        assert!(check_bind("192.168.1.10", None).is_err());
        assert!(check_bind("0.0.0.0", Some("t")).is_ok());
        assert!(check_bind("0.0.0.0", Some("")).is_err());
    }

    #[tokio::test]
    async fn token_gate_rejects_wrong_token_without_forwarding() {
        let (requests_tx, mut requests_rx) = mpsc::channel::<ControlRequest>(4);
        let (client, server) = tokio::io::duplex(4096);
        let (client_reader, mut client_writer) = tokio::io::split(client);
        let (server_reader, server_writer) = tokio::io::split(server);
        let session = tokio::spawn(serve_lines_with_auth(
            server_reader,
            server_writer,
            requests_tx,
            Some("s3cret"),
        ));
        let mut lines = BufReader::new(client_reader).lines();
        client_writer.write_all(b"wrong\n").await.unwrap();
        let reply = lines.next_line().await.unwrap().expect("reply must arrive");
        assert!(reply.contains("\"ok\":false"));
        assert!(
            requests_rx.try_recv().is_err(),
            "rejected connection forwards nothing"
        );
        assert!(!session.await.unwrap(), "gate reports rejection");
    }

    #[tokio::test]
    async fn token_gate_admits_correct_token_then_serves() {
        let (requests_tx, mut requests_rx) = mpsc::channel::<ControlRequest>(4);
        let (client, server) = tokio::io::duplex(4096);
        let (client_reader, mut client_writer) = tokio::io::split(client);
        let (server_reader, server_writer) = tokio::io::split(server);
        let session = tokio::spawn(serve_lines_with_auth(
            server_reader,
            server_writer,
            requests_tx,
            Some("s3cret"),
        ));
        let mut lines = BufReader::new(client_reader).lines();
        client_writer
            .write_all(b"s3cret\n{\"cmd\":\"set-bpm\",\"bpm\":88}\n")
            .await
            .unwrap();
        let request = requests_rx.recv().await.expect("command must arrive");
        assert!(matches!(
            request.command,
            ControlCommand::SetBpm { bpm: 88 }
        ));
        request.reply.send(ControlReply::ok()).unwrap();
        let reply = lines.next_line().await.unwrap().expect("reply must arrive");
        assert_eq!(reply, "{\"ok\":true}");
        session.abort();
    }

    #[tokio::test]
    async fn overlong_control_lines_are_refused_loudly() {
        // TCP framing is bounded: a line past MAX_CONTROL_LINE_BYTES fails
        // its own reply and closes the session instead of growing a buffer.
        let (requests_tx, mut requests_rx) = mpsc::channel::<ControlRequest>(4);
        let (client, server) = tokio::io::duplex(65536);
        let (client_reader, mut client_writer) = tokio::io::split(client);
        let (server_reader, server_writer) = tokio::io::split(server);
        let session = tokio::spawn(serve_lines_with_auth(
            server_reader,
            server_writer,
            requests_tx,
            None,
        ));
        let mut lines = BufReader::new(client_reader).lines();
        let huge = "x".repeat(super::MAX_CONTROL_LINE_BYTES + 16);
        client_writer
            .write_all(format!("{huge}\n").as_bytes())
            .await
            .unwrap();
        let reply = lines.next_line().await.unwrap().expect("reply must arrive");
        assert!(reply.contains("\"ok\":false"), "overlong line fails loudly");
        assert!(
            requests_rx.try_recv().is_err(),
            "overlong line forwards nothing"
        );
        session.abort();
    }

    #[tokio::test]
    async fn listen_reports_bind_failures_to_the_caller() {
        // Readiness is declared after `listen` succeeds, so a second bind of
        // the same port must fail here — never as a silent spawned task.
        let first = super::listen("127.0.0.1", 0, None)
            .await
            .expect("bind works");
        let port = first.local_addr().expect("bound port is known").port();
        let second = super::listen("127.0.0.1", port, None).await;
        assert!(second.is_err(), "double bind fails loudly, not silently");
        drop(first);
    }

    #[tokio::test]
    async fn auth_line_has_a_deadline() {
        // A connection that never presents its token line must not linger:
        // the gate rejects it after CONTROL_AUTH_TIMEOUT.
        let (requests_tx, _requests_rx) = mpsc::channel::<ControlRequest>(4);
        let (client, server) = tokio::io::duplex(4096);
        let (_client_reader, _client_writer) = tokio::io::split(client);
        let (server_reader, server_writer) = tokio::io::split(server);
        let admitted = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            serve_lines_with_auth(server_reader, server_writer, requests_tx, Some("s3cret")),
        )
        .await
        .expect("auth gate must settle without a token line");
        assert!(!admitted, "silent connection is rejected, never parked");
    }

    #[tokio::test]
    async fn control_session_round_trip() {
        // In-memory duplex: the same session logic `serve` runs per TCP
        // connection, without needing a listening socket.
        let (requests_tx, mut requests_rx) = mpsc::channel::<ControlRequest>(4);
        let (client, server) = tokio::io::duplex(4096);
        let (client_reader, mut client_writer) = tokio::io::split(client);
        let (server_reader, server_writer) = tokio::io::split(server);
        let session = tokio::spawn(serve_lines_with_auth(
            server_reader,
            server_writer,
            requests_tx,
            None,
        ));
        let mut lines = BufReader::new(client_reader).lines();
        client_writer
            .write_all(b"{\"cmd\":\"set-bpm\",\"bpm\":88}\n")
            .await
            .unwrap();
        let request = requests_rx.recv().await.expect("command must arrive");
        assert!(matches!(
            request.command,
            ControlCommand::SetBpm { bpm: 88 }
        ));
        request.reply.send(ControlReply::ok()).unwrap();
        let reply = lines.next_line().await.unwrap().expect("reply must arrive");
        assert_eq!(reply, "{\"ok\":true}");
        client_writer
            .write_all(b"{\"cmd\":\"nope\"}\n")
            .await
            .unwrap();
        // Unknown commands fail on their own reply without killing the session.
        let _ = requests_rx
            .try_recv()
            .expect_err("unknown command must not forward");
        let reply = lines.next_line().await.unwrap().expect("reply must arrive");
        assert!(reply.contains("\"ok\":false"));
        session.abort();
    }

    #[test]
    fn rate_validation_rejects_absurd_values() {
        let mut config = SimConfig::default();
        assert!(apply_rates(&mut config, Some(0.0), None, None).is_err());
        assert!(apply_rates(&mut config, None, None, Some(0)).is_err());
        assert!(apply_rates(&mut config, None, None, Some(168)).is_err());
        assert!(apply_rates(&mut config, Some(2.0), Some(4.0), Some(65)).is_ok());
        assert_eq!(config.hr_hz, 2.0);
        assert_eq!(config.ecg_frames_per_sec, 4.0);
        assert_eq!(config.ecg_frame_samples, 65);
    }

    #[test]
    fn rate_changes_apply_atomically() {
        // A request with one good and one bad rate must change nothing:
        // a half-applied rate set is a silent partial mutation.
        let mut config = SimConfig::default();
        let before = config.clone();
        assert!(apply_rates(&mut config, Some(2.0), None, Some(168)).is_err());
        assert_eq!(config.hr_hz, before.hr_hz);
        assert_eq!(config.ecg_frames_per_sec, before.ecg_frames_per_sec);
        assert_eq!(config.ecg_frame_samples, before.ecg_frame_samples);
    }
}
