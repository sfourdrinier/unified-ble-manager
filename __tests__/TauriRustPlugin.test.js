'use strict'

const fs = require('fs')
const path = require('path')

const root = path.resolve(__dirname, '..')
const read = relative => fs.readFileSync(path.join(root, relative), 'utf8')

describe('Tauri v2 Rust plugin boundary', () => {
  test('ships a publishable plugin crate with generated command permissions', () => {
    const cargo = read('native/tauri/Cargo.toml')
    const build = read('native/tauri/build.rs')
    const permissions = read('native/tauri/permissions/default.toml')

    expect(cargo).toContain('name = "tauri-plugin-unified-ble-manager"')
    expect(cargo).toContain('tauri = { version = "2"')
    expect(build).toContain('tauri_plugin::Builder')
    expect(build).toContain('"invoke"')
    expect(permissions).toContain('"allow-invoke"')
  })

  test('authenticates the invoking webview in Rust and never accepts caller identity as request data', () => {
    const commands = read('native/tauri/src/commands.rs')

    expect(commands).toContain('WebviewWindow<R>')
    expect(commands).toContain('AuthenticatedCaller::from_window')
    expect(commands).toContain('Option<JavaScriptChannelId>')
    expect(commands).toContain('IpcValue::from_wire')
    expect(commands).toContain('response.into_wire()')
    expect(commands).not.toMatch(/authenticatedClientId.*request/i)
  })

  test('preserves byte identity through a typed Rust wire value and encoded event sink', () => {
    const wire = read('native/tauri/src/wire.rs')
    const plugin = read('native/tauri/src/lib.rs')

    expect(wire).toContain('pub enum IpcValue')
    expect(wire).toContain('Bytes(Vec<u8>)')
    expect(wire).toContain('$__unifiedBleBytesV2')
    expect(wire).toContain('pub struct IpcEventSink')
    expect(plugin).toContain('request: IpcValue')
    expect(plugin).toContain('event_sink: Option<IpcEventSink>')
  })

  test('binds the event sink once at attach and never rebinds it per request', () => {
    const commands = read('native/tauri/src/commands.rs')
    const dispatcher = read('native/tauri/src/btleplug_dispatcher.rs')

    // Only the attach request may carry a channel, and only this one site may
    // turn it into a Rust Channel: a second Channel on the same JS callback id
    // ends the shared callback when dropped and desynchronises message indices.
    expect(commands).toContain('event_channel: Option<JavaScriptChannelId>')
    expect(commands.match(/channel_on\(/g)).toHaveLength(1)
    expect(dispatcher).not.toContain('caller_state.event_sink = event_sink')
    expect(dispatcher).toContain('tauri.bootstrap-event-channel')
  })

  test('provides an injectable dispatcher rather than embedding a second public BLE API', () => {
    const plugin = read('native/tauri/src/lib.rs')

    expect(plugin).toContain('pub trait IpcDispatcher')
    expect(plugin).toContain('pub struct PluginBuilder')
    expect(plugin).toContain('plugin:unified-ble-manager|invoke')
    expect(plugin).not.toContain('BleManager')
  })

  test('scan follows adapter events, polls peripherals as a fallback, and drops observations instead of aborting when the event quota is full', () => {
    const dispatcher = read('native/tauri/src/btleplug_dispatcher.rs')

    expect(dispatcher).toContain('scan_adapter.events()')
    expect(dispatcher).toContain('DeviceDiscovered')
    expect(dispatcher).toContain('drop_if_full')
    expect(dispatcher).toContain('SCAN_POLL_INTERVAL')
    expect(dispatcher).toContain('peripherals()')
    expect(dispatcher).toContain('ubm-btleplug')
    expect(dispatcher).toContain('new_multi_thread')
    expect(dispatcher).toContain('btleplug_runtime().spawn')
    expect(dispatcher).toContain('heard')
    expect(dispatcher).toContain('adapter.adapter_state()')
  })

  test('runs Rust formatting, tests, and a clippy warning gate in CI', () => {
    const workflow = read('.github/workflows/ci.yml')

    expect(workflow).toContain('cargo fmt --manifest-path native/tauri/Cargo.toml -- --check')
    expect(workflow).toContain('cargo test --manifest-path native/tauri/Cargo.toml')
    expect(workflow).toContain('cargo clippy --manifest-path native/tauri/Cargo.toml -- -D warnings')
  })
})
