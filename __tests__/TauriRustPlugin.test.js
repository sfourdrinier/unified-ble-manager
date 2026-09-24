'use strict'

const fs = require('fs')
const crypto = require('crypto')
const os = require('os')
const path = require('path')
const { spawnSync } = require('child_process')

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

  test('keeps security permissions distinct and enforced by the Rust command boundary', () => {
    const securityPermissions = read('native/tauri/permissions/security.toml')
    const commands = read('native/tauri/src/commands.rs')
    const build = read('native/tauri/build.rs')

    expect(securityPermissions).toContain('allow-security-state')
    expect(securityPermissions).toContain('allow-security-pair')
    expect(securityPermissions).toContain('allow-security-cancel-pairing')
    expect(securityPermissions).toContain('allow-security-unpair')
    expect(securityPermissions).toContain('allow-security-custom-ceremony')
    expect(read('native/tauri/permissions/default.toml')).not.toContain('allow-security-unpair')
    expect(read('native/tauri/permissions/default.toml')).not.toContain('allow-security-custom-ceremony')
    expect(commands).toContain('CommandScope')
    expect(commands).toContain('SecurityPermission')
    expect(commands).toContain('.allows()')
    expect(build).toContain('global_scope_schema')
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

  test('scan executes the shared core, keeps quota-drop/schema/runtime/heard behavior, and carries no second scan authority', () => {
    const dispatcher = read('native/tauri/src/btleplug_dispatcher.rs')
    const executor = read('crates/ubm-desktop/src/executor.rs')

    // R03 contract update (justified): scan admission, duplicate/merge
    // policy, and observation production moved to the shared core
    // (CoreAuthority/DesktopCore). The dispatcher-side adapter event
    // stream, peripheral poll fallback, and scan policy are deleted with
    // the second scheduling authority — the negative pins below prove
    // they cannot silently return. Preserved behavior stays pinned:
    // quota-drop (never abort), wire schema v2, the retained runtime
    // shape and identity, the heard peer count, and the passive
    // power-state read.
    expect(dispatcher).toContain('.start_scan(&key')
    expect(dispatcher).toContain('take_advertisement')
    expect(dispatcher).toContain('CoreAuthority')
    expect(dispatcher).toContain('core_scan_observation')
    expect(dispatcher).toContain('Err(error) if error.code == BleErrorCode::StreamQuota')
    expect(dispatcher).toContain('("schemaVersion", number(2))')
    expect(dispatcher).not.toContain('scan_adapter.events()')
    expect(dispatcher).not.toContain('DeviceDiscovered')
    expect(dispatcher).not.toContain('SCAN_POLL_INTERVAL')
    expect(dispatcher).not.toContain('for service in peripheral.services()')
    // Runtime construction moved to the shared desktop executor
    // (HOST-DESKTOP extraction, then the F01 authority migration from the
    // tauri-local seam to the real `ubm-desktop` crate dependency); the
    // dispatcher must delegate and the executor must preserve the retained
    // runtime shape and identity. F01 contract update (justified): only the
    // owner moved (same thread, same two workers) — the delegation
    // requirement itself is unchanged and still pinned here.
    expect(dispatcher).toContain('ubm_desktop::executor::desktop_runtime()')
    expect(executor).toContain('new_multi_thread')
    expect(executor).toContain('worker_threads(2)')
    expect(executor).toContain('ubm-btleplug')
    expect(executor).toContain('ubm-btleplug-worker')
    expect(dispatcher).toContain('fn btleplug_runtime()')
    // Finding 43: attachment identity and adapter.state come from the one
    // shared central (its own adapter selection, ambiguity refused); the
    // plugin opens no second btleplug Manager (a second CBCentralManager).
    expect(dispatcher).toContain('DesktopCentral::open_btleplug(profile)')
    expect(dispatcher).toContain('authority.attachment()')
    expect(dispatcher).toContain('heard')
    expect(dispatcher).toContain('authority.adapter_state(ctl)')
    expect(dispatcher).not.toContain('Manager::new()')
    expect(dispatcher).not.toContain('open_btleplug_adapter')
  })

  test('preserves native scan emission diagnostics on the shared terminal contract', () => {
    const dispatcher = read('native/tauri/src/btleplug_dispatcher.rs')

    expect(dispatcher).toContain('Err(error) => ("source-failed", Some(error))')
    expect(dispatcher).toMatch(/"source-failed",\s+Some\(&error\)/)
    expect(dispatcher).toContain('fn normalized_error(&self) -> IpcValue')
    expect(dispatcher).toContain('item.insert("error".to_owned(), error.normalized_error())')
    // R03 (restored contract): forwarder failures thread the real error —
    // verbatim core verdicts via from_core, never None, never silent —
    // while quota-drop still sheds observations without aborting the scan.
    expect(dispatcher).toContain('DispatchError::from_core(&error)')
    expect(dispatcher).toContain('error.code == BleErrorCode::StreamQuota')
  })

  test('runs Rust formatting, tests, and a clippy warning gate in CI', () => {
    const workflow = read('.github/workflows/ci.yml')

    expect(workflow).toContain('cargo fmt --manifest-path native/tauri/Cargo.toml -- --check')
    expect(workflow).toContain('cargo test --manifest-path native/tauri/Cargo.toml')
    expect(workflow).toContain('cargo clippy --manifest-path native/tauri/Cargo.toml -- -D warnings')
  })

  test('uses one Tauri Linux dependency installer in every workflow', () => {
    const workflow = read('.github/workflows/ci.yml')
    const publish = read('.github/workflows/publish.yml')
    const installerPath = 'scripts/ci/install-linux-native-system-dependencies.sh'
    const installer = read(installerPath)
    const installerCommand = `bash ${installerPath} tauri`
    const workflowFiles = fs
      .readdirSync(path.join(root, '.github', 'workflows'))
      .filter(file => file.endsWith('.yml') || file.endsWith('.yaml'))
    const tauriJob = workflow.slice(workflow.indexOf('  tauri-plugin:'), workflow.indexOf('  package:'))
    const packageJob = workflow.slice(workflow.indexOf('  package:'), workflow.indexOf('  contracts:'))
    const publishJob = publish.slice(publish.indexOf('  publish:'))

    for (const required of [
      'libwebkit2gtk-4.1-dev',
      'build-essential',
      'curl',
      'wget',
      'file',
      'libxdo-dev',
      'libssl-dev',
      'libayatana-appindicator3-dev',
      'librsvg2-dev',
      'libdbus-1-dev',
      'pkg-config'
    ]) {
      expect(installer).toContain(required)
      expect(installer.split(/\r?\n/).filter(line => line.trim() === required)).toHaveLength(1)
    }

    expect(tauriJob).toContain(installerCommand)
    expect(packageJob).toContain(installerCommand)
    expect(publishJob).toContain(installerCommand)
    expect(workflow).toContain(`bash ${installerPath} bluez`)
    expect(publish).toContain(`bash ${installerPath} desktop-prebuild`)
    for (const file of workflowFiles) {
      const contents = read(path.join('.github', 'workflows', file))
      expect(contents).not.toContain('apt-get install')
      expect(contents).not.toContain('libwebkit2gtk-4.1-dev')
    }
  })

  test('runs the external packed Tauri proof immediately after prepack and before expensive gates', () => {
    const workflow = read('.github/workflows/ci.yml')
    const publish = read('.github/workflows/publish.yml')
    const packageJob = workflow.slice(workflow.indexOf('  package:'), workflow.indexOf('  contracts:'))
    const publishJob = publish.slice(publish.indexOf('  publish:'))

    const assertFailFastOrder = (job, nextExpensiveStep) => {
      const prepack = job.indexOf('- name: Build package artifacts')
      const tauriProof = job.indexOf('- name: Packed external Tauri Cargo consumer proof')
      const expensiveGate = job.indexOf(nextExpensiveStep)

      expect(prepack).toBeGreaterThanOrEqual(0)
      expect(tauriProof).toBeGreaterThan(prepack)
      expect(expensiveGate).toBeGreaterThan(tauriProof)
    }

    assertFailFastOrder(packageJob, '- name: Generated artifacts match the sources')
    assertFailFastOrder(publishJob, '- name: Build 4.0 Web Bluetooth public example')
    expect(read('scripts/ci/tauri-packed-consumer-check.js')).toContain("'cargo', 'build'")
    expect(read('scripts/ci/tauri-packed-consumer-check.js')).toContain('tauri::generate_context!()')
    expect(read('scripts/ci/tauri-packed-consumer-check.js')).toContain('tauriCargoRecipe()')
    expect(publishJob).toContain('check-tauri-pack-binding.js')
    expect(publishJob.indexOf('check-tauri-pack-binding.js')).toBeGreaterThan(
      publishJob.indexOf('Generate the exact canonical npm publish tarball')
    )
  })

  test('rejects a linked Tauri proof for different publish tarball bytes', () => {
    const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-tauri-binding-test-'))
    try {
      const tarball = path.join(directory, 'package.tgz')
      const receipt = path.join(directory, 'receipt.json')
      const manifest = require('../package.json')
      const run = () => spawnSync(process.execPath, [
        path.join(root, 'scripts/ci/check-tauri-pack-binding.js'),
        '--receipt', receipt,
        '--tarball', tarball
      ], { cwd: root, encoding: 'utf8' })
      fs.writeFileSync(tarball, 'packed bytes')
      fs.writeFileSync(receipt, JSON.stringify({
        package: `${manifest.name}@${manifest.version}`,
        tarballSha256: crypto.createHash('sha256').update('packed bytes').digest('hex'),
        proof: 'linked-tauri-application'
      }))
      expect(run().status).toBe(0)
      fs.writeFileSync(tarball, 'different bytes')
      expect(run().status).not.toBe(0)
    } finally {
      fs.rmSync(directory, { recursive: true, force: true })
    }
  })
})
