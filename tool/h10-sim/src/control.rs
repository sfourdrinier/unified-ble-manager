//! JSON-lines TCP control port: live configuration and fault injection.
//!
//! One JSON object per line on stdin of the socket; one JSON reply per line.
//! Unknown commands and invalid arguments are answered with
//! `{"ok":false,"error":"…"}` and logged — never silently ignored.

use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
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

/// Commands accepted on the control port.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum ControlCommand {
    /// Set the heart rate: `{"cmd":"set-bpm","bpm":96}`.
    SetBpm { bpm: u8 },
    /// Start or stop advertising: `{"cmd":"set-advertising","on":false}`.
    SetAdvertising { on: bool },
    /// Tear down advertising (and, on BlueZ, the GATT application behind it,
    /// which drops the current link).
    DropLink,
    /// Stop notifying while keeping the link up (link-loss `silent` fault).
    SetSilent { on: bool },
    /// Fail the next PMD command with a status code, e.g. `{"cmd":"reject-next-pmd","status":3}`.
    RejectNextPmd { status: u8 },
    /// Clear a pending PMD fault without firing it.
    ClearPmdFault,
    /// Set the battery level now: `{"cmd":"set-battery","level":15}` (0–100).
    SetBattery { level: u8 },
    /// Report sensor contact lost or detected: `{"cmd":"set-contact","detected":false}`.
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
    ("drop-link", "tear down advertising and the live link"),
    ("set-silent", "stop notifying while keeping the link up"),
    (
        "reject-next-pmd",
        "fail the next PMD command with a status code",
    ),
    ("clear-pmd-fault", "disarm a pending PMD fault"),
    ("set-rates", "change HR/ECG stream rates"),
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

/// Validates a rate-change request against the simulator config.
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
        config.hr_hz = rate;
    }
    if let Some(rate) = ecg_frames_per_sec {
        if !(0.5..=10.0).contains(&rate) {
            return Err(format!("ecgFramesPerSec {rate} is out of range 0.5..=10.0"));
        }
        config.ecg_frames_per_sec = rate;
    }
    if let Some(samples) = ecg_frame_samples {
        if !(1..=167).contains(&samples) {
            return Err(format!("ecgFrameSamples {samples} is out of range 1..=167"));
        }
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

/// Serves the control port until the listener errors fatally. Every connection
/// is handled independently; a malformed line fails only its own reply. When
/// `token` is set, each connection's first line must equal it (never logged).
pub async fn serve(
    bind: String,
    port: u16,
    token: Option<String>,
    requests: mpsc::Sender<ControlRequest>,
) -> Result<(), String> {
    check_bind(&bind, token.as_deref())?;
    let listener = TcpListener::bind((bind.clone(), port))
        .await
        .map_err(|error| format!("cannot bind control port {bind}:{port}: {error}"))?;
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
async fn serve_lines_with_auth(
    reader: impl AsyncRead + Unpin,
    mut writer: impl AsyncWrite + Unpin,
    requests: mpsc::Sender<ControlRequest>,
    expected_token: Option<&str>,
) -> bool {
    let mut lines = BufReader::new(reader).lines();
    if let Some(expected) = expected_token {
        let admitted = match lines.next_line().await {
            Ok(Some(first)) => first.trim_end_matches(['\r', '\n']) == expected,
            _ => false,
        };
        if !admitted {
            let _ = writer
                .write_all(b"{\"ok\":false,\"error\":\"auth failed\"}\n")
                .await;
            return false;
        }
    }
    while let Ok(Some(line)) = lines.next_line().await {
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
}
