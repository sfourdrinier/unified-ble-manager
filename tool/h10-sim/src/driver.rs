//! Driver client: the simulator joins the shared test driver
//! (`examples-shared/driver`, protocol `ubm-test-driver/1`) as a new host
//! kind `peripheral-sim`.
//!
//! The sim connects OUT to the driver server (`--driver ws://host:port/path`)
//! and exposes its controls as scenario commands on the `sim-control`
//! scenario. Command parsing reuses [`crate::control`] — the hello advertises
//! the single [`control::COMMANDS`](crate::control::COMMANDS) table, so the
//! control port and the driver never drift apart. Protocol types are defined
//! once here in Rust and checked against the TypeScript protocol by the
//! `driver-hello.json` fixture test (see `tests/`).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::protocol::Message;

use crate::control::{self, ControlCommand, ControlReply, ControlRequest};

/// Wire protocol version (must equal `TEST_DRIVER_PROTOCOL` in protocol.ts).
pub const PROTOCOL: &str = "ubm-test-driver/1";
/// Host kind this client hellos as (must be in `HOST_KINDS` in protocol.ts).
pub const HOST_KIND: &str = "peripheral-sim";
/// Scenario id exposing the simulator controls.
pub const SCENARIO: &str = "sim-control";

/// First reconnect wait; the schedule doubles per consecutive failure.
pub const RECONNECT_BASE_SECS: u64 = 1;
/// Upper bound of the exponential schedule ("about 30s").
pub const RECONNECT_CAP_SECS: u64 = 30;
/// Jitter added on top of the schedule is always below this (see [`jitter_ms`]).
pub const RECONNECT_JITTER_MAX_MS: u64 = 1000;

/// Backoff schedule for driver reconnects, with `attempt` counting
/// consecutive failures from 1: 1s, 2s, 4s, 8s, 16s, then 30s forever.
/// Pure, so the schedule is unit-pinned without sleeping on real time.
pub fn reconnect_delay(attempt: u64) -> Duration {
    let shift = attempt.saturating_sub(1).min(5);
    Duration::from_secs((RECONNECT_BASE_SECS << shift).min(RECONNECT_CAP_SECS))
}

/// Deterministic jitter in `[0, RECONNECT_JITTER_MAX_MS)` ms from a seed —
/// production passes [`now_ms`], tests pass fixed seeds. A splitmix64
/// finalizer spreads neighbouring seeds; no RNG dependency for one number.
pub fn jitter_ms(seed: u64) -> u64 {
    let mut x = seed.wrapping_add(0x9E3779B97F4A7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D049BB133111EB);
    (x ^ (x >> 31)) % RECONNECT_JITTER_MAX_MS
}

/// Full wait before the next attempt: schedule plus jitter.
pub fn reconnect_wait(attempt: u64, seed: u64) -> Duration {
    reconnect_delay(attempt) + Duration::from_millis(jitter_ms(seed))
}

/// How one driver session ended. Transport trouble rejoins with a fresh
/// hello; only a gone simulator loop ends the task.
enum SessionEnd {
    Fatal(String),
    Rejoin(String),
}

/// One handled server frame: the next event sequence number, plus the new
/// host id when the frame was the welcome to this registration.
struct FrameOutcome {
    seq: u64,
    welcome_host: Option<String>,
}

type WsSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
type WsSink = futures::stream::SplitSink<WsSocket, Message>;
type WsRead = futures::stream::SplitStream<WsSocket>;

/// Builds the driver hello. Fails closed on the server side for any drift:
/// unknown host kinds and protocol mismatches are refused.
pub fn hello_json(host_label: &str) -> Value {
    let commands: Vec<Value> = control::COMMANDS
        .iter()
        .map(|(name, description)| {
            json!({
                "name": name,
                "label": name,
                "description": description,
                "presets": [],
                "acceptsDevice": false,
            })
        })
        .collect();
    json!({
        "type": "hello",
        "protocol": PROTOCOL,
        "host": HOST_KIND,
        "platform": std::env::consts::OS,
        "backend": "tool/h10-sim",
        "model": host_label,
        "osVersion": std::env::consts::OS,
        "appBuild": {},
        "scenarios": [{
            "id": SCENARIO,
            "title": "H10 simulator control",
            "description": "Live simulator controls (bpm, battery, advertising, link faults, PMD faults).",
            "commands": commands,
        }],
    })
}

/// A decoded server message we act on.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerInbound {
    Welcome {
        host_id: String,
    },
    Command {
        id: String,
        scenario: String,
        command: String,
        args: Value,
    },
}

/// Decodes one server text frame using the same field names as
/// `decodeServerMessage` in protocol.ts. Anything else is a loud Err.
pub fn decode_server_text(text: &str) -> Result<ServerInbound, String> {
    let value: Value =
        serde_json::from_str(text).map_err(|error| format!("driver: invalid JSON: {error}"))?;
    match value.get("type").and_then(|kind| kind.as_str()) {
        Some("welcome") => {
            let protocol = value
                .get("protocol")
                .and_then(|protocol| protocol.as_str())
                .unwrap_or("");
            if protocol != PROTOCOL {
                return Err(format!(
                    "driver: protocol mismatch: expected {PROTOCOL}, received {protocol}"
                ));
            }
            let host_id = value
                .get("hostId")
                .and_then(|id| id.as_str())
                .ok_or_else(|| "driver: welcome requires a string hostId".to_string())?;
            Ok(ServerInbound::Welcome {
                host_id: host_id.to_string(),
            })
        }
        Some("command") => {
            let id = value
                .get("id")
                .and_then(|id| id.as_str())
                .filter(|id| !id.is_empty())
                .ok_or_else(|| "driver: command requires a non-empty string id".to_string())?;
            let scenario = value
                .get("scenario")
                .and_then(|scenario| scenario.as_str())
                .ok_or_else(|| "driver: command requires a string scenario".to_string())?;
            let command = value
                .get("command")
                .and_then(|command| command.as_str())
                .ok_or_else(|| "driver: command requires a string command".to_string())?;
            let args = match value.get("args") {
                None => Value::Object(Default::default()),
                Some(args) if args.is_object() => args.clone(),
                Some(_) => {
                    return Err("driver: command args must be a JSON object".to_string());
                }
            };
            Ok(ServerInbound::Command {
                id: id.to_string(),
                scenario: scenario.to_string(),
                command: command.to_string(),
                args,
            })
        }
        other => Err(format!("driver: unknown server message type {other:?}")),
    }
}

/// Builds a [`ControlCommand`] from a driver command name plus its args
/// object, reusing the control-port parser exactly (the name becomes `cmd`).
pub fn control_from_driver(command: &str, args: &Value) -> Result<ControlCommand, String> {
    let mut map = args.as_object().cloned().unwrap_or_default();
    map.insert("cmd".to_string(), Value::from(command));
    serde_json::from_value(Value::Object(map))
        .map_err(|error| format!("driver: unknown sim-control command {command:?}: {error}"))
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

fn result_message(id: &str, scenario: &str, command: &str, result: &Value) -> Value {
    json!({
        "type": "result",
        "id": id,
        "scenario": scenario,
        "command": command,
        "atMs": now_ms(),
        "result": result,
    })
}

fn error_message(
    id: Option<&str>,
    scenario: Option<&str>,
    command: Option<&str>,
    code: &str,
    message: String,
) -> Value {
    json!({
        "type": "error",
        "id": id,
        "scenario": scenario,
        "command": command,
        "atMs": now_ms(),
        "error": {"code": code, "message": message, "detail": Value::Null},
    })
}

fn event_message(seq: u64, host_label: &str, kind: &str, data: &Value) -> Value {
    json!({
        "type": "event",
        "event": {
            "scenario": SCENARIO,
            "seq": seq,
            "atMs": now_ms(),
            "host": host_label,
            "kind": kind,
            "data": data,
        },
    })
}

async fn send_json(sink: &mut WsSink, value: &Value) -> Result<(), String> {
    let text =
        serde_json::to_string(value).map_err(|error| format!("driver: cannot encode: {error}"))?;
    sink.send(Message::Text(text.into()))
        .await
        .map_err(|error| format!("driver: send failed: {error}"))
}

/// Joins the driver server at `url`: hellos, dispatches `sim-control`
/// commands through the simulator loop, and streams log events. Only returns
/// when the simulator loop itself is gone (fatal [`SessionEnd::Fatal`]);
/// anything else rejoins — see [`run_with_sleep`].
pub async fn run(
    url: String,
    host_label: String,
    commands: mpsc::Sender<ControlRequest>,
    events: mpsc::Receiver<Value>,
) -> Result<(), String> {
    run_with_sleep(url, host_label, commands, events, tokio::time::sleep).await
}

/// The rejoin loop: connects, serves one session, and — unlike the old
/// behaviour that gave up after 3 tries and never retried a dropped session
/// (FXG) — rejoins with bounded exponential backoff forever, since a
/// long-lived test peripheral must heal itself.
///
/// At most one connection is ever open: the next hello goes out only after
/// the previous socket closed, so the hub (which deletes the registration on
/// close) never holds two live registrations for this sim. Simulator control
/// state (mode, faults, profile, run record) lives in the sim loop behind
/// `commands` and is untouched by a rejoin — nothing is reset, so nothing
/// needs re-announcing beyond the fresh hello. The event sequence keeps
/// counting across sessions so `(scenario, seq)` pairs never repeat. Every
/// attempt, failure and re-registration is logged with its attempt number
/// and delay: a dropped connection that reports nothing is the expensive
/// kind of bug.
async fn run_with_sleep<Sleep, SleepFuture>(
    url: String,
    host_label: String,
    commands: mpsc::Sender<ControlRequest>,
    mut events: mpsc::Receiver<Value>,
    sleep: Sleep,
) -> Result<(), String>
where
    Sleep: Fn(Duration) -> SleepFuture,
    SleepFuture: std::future::Future<Output = ()>,
{
    let mut attempt: u64 = 0;
    let mut consecutive_failures: u64 = 0;
    let mut registrations: u64 = 0;
    let mut seq: u64 = 1;
    loop {
        attempt += 1;
        eprintln!("h10-sim: driver: connect attempt {attempt} to {url}");
        match connect_once(&url).await {
            Ok((sink, stream)) => {
                consecutive_failures = 0;
                registrations += 1;
                eprintln!(
                    "h10-sim: driver: connected to {url} (attempt {attempt}); sending hello (registration {registrations})"
                );
                match serve_session(
                    stream,
                    sink,
                    &host_label,
                    &commands,
                    &mut events,
                    registrations,
                    &mut seq,
                )
                .await
                {
                    SessionEnd::Fatal(message) => return Err(message),
                    SessionEnd::Rejoin(reason) => {
                        consecutive_failures += 1;
                        let wait = reconnect_wait(consecutive_failures, now_ms());
                        eprintln!(
                            "h10-sim: driver: session {registrations} ended: {reason}; rejoining in {}ms (next attempt {})",
                            wait.as_millis(),
                            attempt + 1,
                        );
                        sleep(wait).await;
                    }
                }
            }
            Err(error) => {
                consecutive_failures += 1;
                let wait = reconnect_wait(consecutive_failures, now_ms());
                eprintln!(
                    "h10-sim: driver: connect attempt {attempt} to {url} failed: {error}; retrying in {}ms (next attempt {})",
                    wait.as_millis(),
                    attempt + 1,
                );
                sleep(wait).await;
            }
        }
    }
}

/// One TCP+TLS+WebSocket handshake. Loud Err on refusal — the rejoin loop,
/// not the single attempt, owns recovery.
async fn connect_once(url: &str) -> Result<(WsSink, WsRead), String> {
    match tokio_tungstenite::connect_async(url).await {
        Ok((socket, _)) => Ok(socket.split()),
        Err(error) => Err(error.to_string()),
    }
}

/// Serves one connected session: hellos, dispatches `sim-control` commands,
/// streams log events. Returns how the session ended; transport trouble is
/// [`SessionEnd::Rejoin`], never a silent drop.
#[allow(clippy::too_many_arguments)]
async fn serve_session(
    mut stream: WsRead,
    mut sink: WsSink,
    host_label: &str,
    commands: &mpsc::Sender<ControlRequest>,
    events: &mut mpsc::Receiver<Value>,
    registration: u64,
    seq: &mut u64,
) -> SessionEnd {
    if let Err(error) = send_json(&mut sink, &hello_json(host_label)).await {
        return SessionEnd::Rejoin(format!("hello send failed: {error}"));
    }
    loop {
        tokio::select! {
            incoming = stream.next() => {
                match incoming {
                    None => return SessionEnd::Rejoin("server closed the connection".to_string()),
                    Some(Err(error)) => return SessionEnd::Rejoin(format!("receive failed: {error}")),
                    Some(Ok(Message::Close(_))) => {
                        return SessionEnd::Rejoin("server closed the connection".to_string());
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        if let Err(error) = sink.send(Message::Pong(payload)).await {
                            return SessionEnd::Rejoin(format!("pong failed: {error}"));
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        match handle_server_text(&text, &mut sink, commands, *seq).await {
                            Ok(outcome) => {
                                *seq = outcome.seq;
                                if let Some(host_id) = outcome.welcome_host {
                                    if registration > 1 {
                                        eprintln!("h10-sim: driver: re-registered as {host_id} (registration {registration}); sim control state preserved (mode, faults, profile, run record live in the sim loop, untouched by the rejoin)");
                                    } else {
                                        eprintln!("h10-sim: driver: registered as {host_id} (registration {registration})");
                                    }
                                }
                            }
                            Err(end) => return end,
                        }
                    }
                    Some(Ok(_)) => {}
                }
            }
            Some(event) = events.recv() => {
                let kind = event.get("kind").and_then(|kind| kind.as_str()).unwrap_or("log").to_string();
                if let Err(error) = send_json(&mut sink, &event_message(*seq, host_label, &kind, &event)).await {
                    return SessionEnd::Rejoin(format!("event send failed: {error}"));
                }
                *seq = seq.saturating_add(1);
            }
        }
    }
}

/// Handles one server text frame; returns the next event sequence number
/// plus the welcome host id when this frame (re-)registered us. A broken
/// frame or a dead send ends the session with [`SessionEnd::Rejoin`] — the
/// rejoin loop, never silence, owns what happens next. Only a gone simulator
/// loop is [`SessionEnd::Fatal`]: no reconnect can bring that back.
async fn handle_server_text(
    text: &str,
    sink: &mut WsSink,
    commands: &mpsc::Sender<ControlRequest>,
    seq: u64,
) -> Result<FrameOutcome, SessionEnd> {
    let inbound = decode_server_text(text).map_err(SessionEnd::Rejoin)?;
    let outcome = |welcome_host: Option<String>| FrameOutcome { seq, welcome_host };
    match inbound {
        ServerInbound::Welcome { host_id } => Ok(outcome(Some(host_id))),
        ServerInbound::Command {
            id,
            scenario,
            command,
            args,
        } => {
            if scenario != SCENARIO {
                send_json(
                    sink,
                    &error_message(
                        Some(&id),
                        Some(&scenario),
                        Some(&command),
                        "driver.unknown-scenario",
                        format!("unknown scenario {scenario:?}, only {SCENARIO:?} is served"),
                    ),
                )
                .await
                .map_err(SessionEnd::Rejoin)?;
                return Ok(outcome(None));
            }
            let control = match control_from_driver(&command, &args) {
                Ok(control) => control,
                Err(message) => {
                    send_json(
                        sink,
                        &error_message(
                            Some(&id),
                            Some(&scenario),
                            Some(&command),
                            "sim.unknown-command",
                            message,
                        ),
                    )
                    .await
                    .map_err(SessionEnd::Rejoin)?;
                    return Ok(outcome(None));
                }
            };
            let (reply_tx, reply_rx) = oneshot::channel::<ControlReply>();
            let name = control_name(&control);
            if commands
                .send(ControlRequest {
                    command: control,
                    reply: reply_tx,
                })
                .await
                .is_err()
            {
                return Err(SessionEnd::Fatal(
                    "driver: simulator loop is gone; ending driver task".to_string(),
                ));
            }
            let reply = reply_rx
                .await
                .unwrap_or_else(|_| ControlReply::failed("simulator loop is gone"));
            let reply_value =
                serde_json::to_value(&reply).unwrap_or(Value::String("unencodable".to_string()));
            if reply.ok {
                send_json(sink, &result_message(&id, &scenario, &name, &reply_value))
                    .await
                    .map_err(SessionEnd::Rejoin)?;
            } else {
                send_json(
                    sink,
                    &error_message(
                        Some(&id),
                        Some(&scenario),
                        Some(&name),
                        "sim.command-failed",
                        reply.error.unwrap_or_else(|| "command failed".to_string()),
                    ),
                )
                .await
                .map_err(SessionEnd::Rejoin)?;
            }
            Ok(outcome(None))
        }
    }
}

/// Command name for result/error echoes, reusing the kebab-case table in
/// [`control`](crate::control) — a new command is added there, never here.
fn control_name(command: &ControlCommand) -> String {
    command.name().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_matches_driver_protocol_shape() {
        let hello = hello_json("h10-sim");
        assert_eq!(hello["type"], "hello");
        assert_eq!(hello["protocol"], "ubm-test-driver/1");
        assert_eq!(hello["host"], "peripheral-sim");
        assert_eq!(hello["backend"], "tool/h10-sim");
        let scenarios = hello["scenarios"].as_array().unwrap();
        assert_eq!(scenarios.len(), 1);
        assert_eq!(scenarios[0]["id"], "sim-control");
        let advertised: Vec<&str> = scenarios[0]["commands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|command| command["name"].as_str().unwrap())
            .collect();
        let expected: Vec<&str> = control::COMMANDS.iter().map(|(name, _)| *name).collect();
        assert_eq!(advertised, expected);
        for command in scenarios[0]["commands"].as_array().unwrap() {
            assert_eq!(command["acceptsDevice"], false);
        }
    }

    #[test]
    fn server_frames_decode_with_ts_field_names() {
        let welcome = decode_server_text(
            r#"{"type":"welcome","protocol":"ubm-test-driver/1","hostId":"h1"}"#,
        )
        .unwrap();
        assert_eq!(
            welcome,
            ServerInbound::Welcome {
                host_id: "h1".to_string()
            }
        );
        assert!(decode_server_text(
            r#"{"type":"welcome","protocol":"ubm-test-driver/2","hostId":"h1"}"#
        )
        .is_err());
        let command = decode_server_text(
            r#"{"type":"command","id":"c1","scenario":"sim-control","command":"set-bpm","args":{"bpm":90}}"#,
        )
        .unwrap();
        assert!(matches!(
            command,
            ServerInbound::Command {
                id,
                ..
            } if id == "c1"
        ));
        assert!(decode_server_text(r#"{"type":"reboot"}"#).is_err());
        assert!(decode_server_text("not json").is_err());
    }

    #[test]
    fn driver_commands_reuse_control_parsing() {
        let command = control_from_driver("set-bpm", &json!({"bpm": 90})).unwrap();
        assert!(matches!(command, ControlCommand::SetBpm { bpm: 90 }));
        assert!(control_from_driver("self-destruct", &json!({})).is_err());
    }

    #[test]
    fn reconnect_schedule_doubles_then_caps() {
        let expected = [1, 2, 4, 8, 16, 30, 30, 30];
        for (index, secs) in expected.iter().enumerate() {
            let attempt = index as u64 + 1;
            assert_eq!(
                reconnect_delay(attempt),
                Duration::from_secs(*secs),
                "attempt {attempt}"
            );
        }
    }

    #[test]
    fn jitter_stays_inside_its_bound_and_spreads() {
        for seed in [0, 1, 42, 123456789, u64::MAX] {
            assert!(
                jitter_ms(seed) < RECONNECT_JITTER_MAX_MS,
                "seed {seed} out of bound"
            );
        }
        let spread: std::collections::HashSet<u64> = (0..100).map(jitter_ms).collect();
        assert!(spread.len() > 50, "jitter does not spread: {spread:?}");
    }

    #[test]
    fn reconnect_wait_is_schedule_plus_bounded_jitter() {
        for attempt in 1..=7u64 {
            let wait = reconnect_wait(attempt, 7);
            assert!(wait >= reconnect_delay(attempt), "attempt {attempt}");
            assert!(
                wait < reconnect_delay(attempt) + Duration::from_millis(RECONNECT_JITTER_MAX_MS),
                "attempt {attempt}"
            );
        }
    }

    /// Reads the next text frame from a fake-hub connection as JSON, failing
    /// loudly on anything else.
    async fn next_text(
        stream: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    ) -> Value {
        use futures::StreamExt;
        match stream.next().await {
            Some(Ok(Message::Text(text))) => {
                serde_json::from_str(&text).expect("hub frame is JSON")
            }
            other => panic!("expected a text frame, got {other:?}"),
        }
    }

    fn text_frame(text: &str) -> Message {
        Message::Text(text.to_string().into())
    }

    /// A refused TCP connection is a loud Err from the single attempt — the
    /// rejoin loop, not the attempt, owns recovery. (Port taken from a just
    /// dropped listener, so nothing is listening there.)
    #[tokio::test]
    async fn refused_connection_is_a_loud_error() {
        let port = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let error = connect_once(&format!("ws://127.0.0.1:{port}/host")).await;
        assert!(error.is_err(), "expected a loud refusal, got {error:?}");
    }

    /// The loop behind `run` with the sleep injected: records waits instead
    /// of sleeping on real time, so the schedule is pinned deterministically.
    fn recording_sleeper(
        waits: std::sync::Arc<std::sync::Mutex<Vec<Duration>>>,
    ) -> impl Fn(Duration) -> std::future::Ready<()> {
        move |wait: Duration| {
            waits.lock().unwrap().push(wait);
            std::future::ready(())
        }
    }

    /// Fails the transport twice (accept then drop, like a dead hub), then
    /// serves: the sim must rejoin on its own with the two scheduled waits,
    /// and the third hello must carry the same identity so the hub replaces
    /// the dead registration instead of accumulating a ghost.
    #[tokio::test]
    async fn fails_twice_then_reregisters_with_backoff() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/host", listener.local_addr().unwrap());
        let (cmd_tx, _cmd_rx) = mpsc::channel::<ControlRequest>(16);
        let (_evt_tx, evt_rx) = mpsc::channel::<Value>(16);
        let waits: std::sync::Arc<std::sync::Mutex<Vec<Duration>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let driver = tokio::spawn(run_with_sleep(
            url,
            "peripheral-sim/test".to_string(),
            cmd_tx,
            evt_rx,
            recording_sleeper(waits.clone()),
        ));

        let scenario = async {
            use futures::SinkExt;
            let mut hellos = Vec::new();
            for _ in 0..3 {
                let (socket, _) = listener.accept().await.unwrap();
                let mut hub = tokio_tungstenite::accept_async(socket).await.unwrap();
                if hellos.len() < 2 {
                    // Dead hub: drop the connection mid-hello.
                    drop(hub);
                    continue;
                }
                hellos.push(next_text(&mut hub).await);
                hub.send(text_frame(
                    r#"{"type":"welcome","protocol":"ubm-test-driver/1","hostId":"h3"}"#,
                ))
                .await
                .unwrap();
                return hellos;
            }
            hellos
        };
        // The dead drops above never yield a hello; the served (third)
        // connection must still hello exactly once.
        let _ = tokio::time::timeout(Duration::from_secs(10), scenario)
            .await
            .expect("rejoin took too long");
        driver.abort();

        let waits = waits.lock().unwrap().clone();
        // Each drop followed a successful connect, which resets the
        // consecutive-failure count — so both waits sit at schedule
        // position 1. Doubling across consecutive failures is pinned by
        // `reconnect_schedule_doubles_then_caps` (pure) and by
        // `consecutive_refusals_double_the_wait` (loop, below).
        assert_eq!(waits.len(), 2, "expected two backoff waits, got {waits:?}");
        for (index, wait) in waits.iter().enumerate() {
            assert!(
                *wait >= reconnect_delay(1),
                "wait {index} below schedule: {wait:?}"
            );
            assert!(
                *wait < reconnect_delay(1) + Duration::from_millis(RECONNECT_JITTER_MAX_MS),
                "wait {index} above schedule+jitter: {wait:?}"
            );
        }
    }

    /// Points the loop at a closed port (the FXG starting condition): every
    /// refusal is loud, the waits double 1s/2s/4s, and the loop never gives
    /// up — the test stops it after the third wait via the sleeper itself,
    /// so no real-time polling.
    #[tokio::test]
    async fn consecutive_refusals_double_the_wait() {
        let port = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let url = format!("ws://127.0.0.1:{port}/host");
        let (cmd_tx, _cmd_rx) = mpsc::channel::<ControlRequest>(16);
        let (_evt_tx, evt_rx) = mpsc::channel::<Value>(16);
        let waits: std::sync::Arc<std::sync::Mutex<Vec<Duration>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
        let waits_in = waits.clone();
        let driver = tokio::spawn(run_with_sleep(
            url,
            "peripheral-sim/test".to_string(),
            cmd_tx,
            evt_rx,
            move |wait: Duration| {
                waits_in.lock().unwrap().push(wait);
                if waits_in.lock().unwrap().len() >= 3 {
                    let _ = stop_tx.try_send(());
                }
                std::future::ready(())
            },
        ));
        tokio::time::timeout(Duration::from_secs(10), stop_rx.recv())
            .await
            .expect("refusals never produced three waits")
            .expect("stop channel closed");
        driver.abort();

        let waits = waits.lock().unwrap().clone();
        assert!(waits.len() >= 3, "expected three waits, got {waits:?}");
        for (index, wait) in waits.iter().take(3).enumerate() {
            let attempt = index as u64 + 1;
            assert!(
                *wait >= reconnect_delay(attempt),
                "wait {index} below schedule: {wait:?}"
            );
            assert!(
                *wait < reconnect_delay(attempt) + Duration::from_millis(RECONNECT_JITTER_MAX_MS),
                "wait {index} above schedule+jitter: {wait:?}"
            );
        }
    }

    /// Drops the first connection after its hello (hub reboot), then serves
    /// the rejoined one: the hello must be re-sent identically (clean
    /// re-registration), and a command on the NEW connection must still be
    /// dispatched through the SAME command channel — control state
    /// (mode, faults, profile, run record) lives in the sim loop behind it
    /// and survives the rejoin unreset.
    #[tokio::test]
    async fn rejoins_after_a_dropped_connection_and_keeps_serving() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/host", listener.local_addr().unwrap());
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<ControlRequest>(16);
        let (evt_tx, evt_rx) = mpsc::channel::<Value>(16);
        let waits: std::sync::Arc<std::sync::Mutex<Vec<Duration>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let driver = tokio::spawn(run_with_sleep(
            url,
            "peripheral-sim/test".to_string(),
            cmd_tx,
            evt_rx,
            recording_sleeper(waits.clone()),
        ));

        let scenario = async {
            use futures::SinkExt;
            // Connection 1: read the hello, then die like a rebooted hub.
            let (socket, _) = listener.accept().await.unwrap();
            let mut hub1 = tokio_tungstenite::accept_async(socket).await.unwrap();
            let hello1 = next_text(&mut hub1).await;
            assert_eq!(hello1["type"], "hello");
            assert_eq!(hello1["host"], "peripheral-sim");
            drop(hub1);

            // Connection 2, opened by the sim itself: a fresh hello with the
            // same identity, so the hub replaces the dead registration.
            // Sequential loop => the old socket closed before this hello.
            let (socket, _) = listener.accept().await.unwrap();
            let mut hub2 = tokio_tungstenite::accept_async(socket).await.unwrap();
            let hello2 = next_text(&mut hub2).await;
            assert_eq!(hello2, hello1, "re-registration changed identity");

            hub2.send(text_frame(
                r#"{"type":"welcome","protocol":"ubm-test-driver/1","hostId":"h2"}"#,
            ))
            .await
            .unwrap();
            hub2
                .send(text_frame(
                    r#"{"type":"command","id":"c1","scenario":"sim-control","command":"set-bpm","args":{"bpm":90}}"#,
                ))
                .await
                .unwrap();
            let request = tokio::time::timeout(Duration::from_secs(5), cmd_rx.recv())
                .await
                .expect("command never reached the sim loop")
                .expect("command channel closed");
            assert_eq!(request.command.name(), "set-bpm");
            request.reply.send(ControlReply::ok()).unwrap();
            let result = next_text(&mut hub2).await;
            assert_eq!(result["type"], "result");
            assert_eq!(result["id"], "c1");

            // Events stream on the rejoined connection as well.
            evt_tx
                .send(json!({"kind": "log", "note": "post-rejoin"}))
                .await
                .unwrap();
            let event = next_text(&mut hub2).await;
            assert_eq!(event["type"], "event");
        };
        tokio::time::timeout(Duration::from_secs(10), scenario)
            .await
            .expect("rejoin scenario took too long");
        driver.abort();

        let waits = waits.lock().unwrap().clone();
        assert_eq!(waits.len(), 1, "expected one backoff wait, got {waits:?}");
        assert!(waits[0] >= reconnect_delay(1));
        assert!(waits[0] < reconnect_delay(1) + Duration::from_millis(RECONNECT_JITTER_MAX_MS));
    }
}
