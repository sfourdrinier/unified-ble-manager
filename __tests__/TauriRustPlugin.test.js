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
    expect(commands).toContain('Channel<Value>')
    expect(commands).toContain('IpcValue::from_wire')
    expect(commands).toContain('response.into_wire()')
    expect(commands).not.toMatch(/authenticatedClientId.*request/i)
  })

  test('preserves byte identity through a typed Rust wire value and encoded event sink', () => {
    const wire = read('native/tauri/src/wire.rs')
    const plugin = read('native/tauri/src/lib.rs')

    expect(wire).toContain('pub enum IpcValue')
    expect(wire).toContain('Bytes(Vec<u8>)')
    expect(wire).toContain('$__unifiedBleBytesV1')
    expect(wire).toContain('pub struct IpcEventSink')
    expect(plugin).toContain('request: IpcValue')
    expect(plugin).toContain('event_sink: IpcEventSink')
  })

  test('provides an injectable dispatcher rather than embedding a second public BLE API', () => {
    const plugin = read('native/tauri/src/lib.rs')

    expect(plugin).toContain('pub trait IpcDispatcher')
    expect(plugin).toContain('pub struct PluginBuilder')
    expect(plugin).toContain('plugin:unified-ble-manager|invoke')
    expect(plugin).not.toContain('BleManager')
  })

  test('runs Rust formatting, tests, and a clippy warning gate in CI', () => {
    const workflow = read('.github/workflows/ci.yml')

    expect(workflow).toContain('cargo fmt --manifest-path native/tauri/Cargo.toml -- --check')
    expect(workflow).toContain('cargo test --manifest-path native/tauri/Cargo.toml')
    expect(workflow).toContain('cargo clippy --manifest-path native/tauri/Cargo.toml -- -D warnings')
  })
})
