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

use std::time::{SystemTime, UNIX_EPOCH};

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

async fn send_json(
    sink: &mut futures::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        Message,
    >,
    value: &Value,
) -> Result<(), String> {
    let text =
        serde_json::to_string(value).map_err(|error| format!("driver: cannot encode: {error}"))?;
    sink.send(Message::Text(text.into()))
        .await
        .map_err(|error| format!("driver: send failed: {error}"))
}

/// Joins the driver server at `url`: hellos, dispatches `sim-control`
/// commands through the simulator loop, and streams log events. Returns when
/// the connection closes or fails — loudly (Err), never a silent drop.
pub async fn run(
    url: String,
    host_label: String,
    commands: mpsc::Sender<ControlRequest>,
    mut events: mpsc::Receiver<Value>,
) -> Result<(), String> {
    let mut attempt = 0;
    let (mut sink, mut stream) = loop {
        attempt += 1;
        match tokio_tungstenite::connect_async(&url).await {
            Ok((socket, _)) => break socket.split(),
            Err(error) => {
                let message = format!("driver: connect attempt {attempt} to {url} failed: {error}");
                eprintln!("h10-sim: {message}");
                if attempt >= 3 {
                    return Err(message);
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    };
    send_json(&mut sink, &hello_json(&host_label)).await?;
    let mut seq: u64 = 1;
    loop {
        tokio::select! {
            incoming = stream.next() => {
                match incoming {
                    None => return Err("driver: server closed the connection".to_string()),
                    Some(Err(error)) => return Err(format!("driver: receive failed: {error}")),
                    Some(Ok(Message::Close(_))) => {
                        return Err("driver: server closed the connection".to_string());
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        sink.send(Message::Pong(payload)).await.map_err(|error| format!("driver: pong failed: {error}"))?;
                    }
                    Some(Ok(Message::Text(text))) => {
                        seq = handle_server_text(&text, &mut sink, &commands, seq).await?;
                    }
                    Some(Ok(_)) => {}
                }
            }
            Some(event) = events.recv() => {
                let kind = event.get("kind").and_then(|kind| kind.as_str()).unwrap_or("log").to_string();
                send_json(&mut sink, &event_message(seq, &host_label, &kind, &event)).await?;
                seq = seq.saturating_add(1);
            }
        }
    }
}

/// Handles one server text frame; returns the next event sequence number.
/// Protocol violations from the server are loud Errs (the connection ends).
async fn handle_server_text(
    text: &str,
    sink: &mut futures::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        Message,
    >,
    commands: &mpsc::Sender<ControlRequest>,
    seq: u64,
) -> Result<u64, String> {
    let inbound = decode_server_text(text)?;
    match inbound {
        ServerInbound::Welcome { .. } => Ok(seq),
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
                .await?;
                return Ok(seq);
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
                    .await?;
                    return Ok(seq);
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
                send_json(
                    sink,
                    &error_message(
                        Some(&id),
                        Some(&scenario),
                        None,
                        "sim.loop-gone",
                        "simulator loop is gone".to_string(),
                    ),
                )
                .await?;
                return Ok(seq);
            }
            let reply = reply_rx
                .await
                .unwrap_or_else(|_| ControlReply::failed("simulator loop is gone"));
            let reply_value =
                serde_json::to_value(&reply).unwrap_or(Value::String("unencodable".to_string()));
            if reply.ok {
                send_json(sink, &result_message(&id, &scenario, &name, &reply_value)).await?;
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
                .await?;
            }
            Ok(seq)
        }
    }
}

/// Command name for result/error echoes (mirrors the kebab-case table).
fn control_name(command: &ControlCommand) -> String {
    match command {
        ControlCommand::SetBpm { .. } => "set-bpm",
        ControlCommand::SetBattery { .. } => "set-battery",
        ControlCommand::SetContact { .. } => "set-contact",
        ControlCommand::PairPolicy { .. } => "pair-policy",
        ControlCommand::LoadProfile { .. } => "load-profile",
        ControlCommand::SetAdvertising { .. } => "set-advertising",
        ControlCommand::DropLink => "drop-link",
        ControlCommand::SetSilent { .. } => "set-silent",
        ControlCommand::RejectNextPmd { .. } => "reject-next-pmd",
        ControlCommand::ClearPmdFault => "clear-pmd-fault",
        ControlCommand::SetRates { .. } => "set-rates",
        ControlCommand::GetState => "get-state",
        ControlCommand::Help => "help",
    }
    .to_string()
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
}
