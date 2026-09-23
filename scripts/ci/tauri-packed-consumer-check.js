#!/usr/bin/env node

'use strict'

const fs = require('fs')
const os = require('os')
const path = require('path')
const { spawnSync } = require('child_process')

const root = path.resolve(__dirname, '../..')
const packageManifest = require(path.join(root, 'package.json'))
const timeoutMs = 15 * 60 * 1000

function run(command, args, options = {}) {
  const cwd = options.cwd || root
  const result = spawnSync(command, args, {
    cwd,
    encoding: 'utf8',
    env: { ...process.env, ...(options.env || {}) },
    shell: false,
    timeout: timeoutMs
  })
  const output = `${result.stdout || ''}${result.stderr || ''}`
  if (result.error?.code === 'ETIMEDOUT') {
    throw new Error(`${command} timed out after ${String(timeoutMs)}ms (cwd: ${cwd})\n${output}`)
  }
  if (result.error) throw new Error(`${command} could not start: ${result.error.message}`)
  if (result.signal !== null) throw new Error(`${command} terminated by ${result.signal} (cwd: ${cwd})\n${output}`)
  if (result.status !== 0) throw new Error(`${command} failed (${String(result.status)}):\n${output}`)
  return output
}

function removeTemporaryDirectory(directory) {
  const resolved = path.resolve(directory)
  const relative = path.relative(path.resolve(os.tmpdir()), resolved)
  if (
    relative === '' ||
    relative === '..' ||
    relative.startsWith(`..${path.sep}`) ||
    !path.basename(resolved).startsWith('ubm-tauri-packed-consumer-')
  ) {
    throw new Error(`Refusing to clean unexpected Tauri consumer directory: ${resolved}`)
  }
  fs.rmSync(resolved, { recursive: true, force: true })
}

const temporaryDirectory = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-tauri-packed-consumer-'))

try {
  const artifacts = path.join(temporaryDirectory, 'artifacts')
  const consumer = path.join(temporaryDirectory, 'consumer')
  const cargoRoot = path.join(consumer, 'src-tauri')
  fs.mkdirSync(artifacts)
  fs.mkdirSync(path.join(cargoRoot, 'src'), { recursive: true })

  run('npm', ['pack', '--ignore-scripts', '--pack-destination', artifacts])
  const tarball = path.join(
    artifacts,
    `${packageManifest.name.replace(/^@/, '').replace('/', '-')}-${packageManifest.version}.tgz`
  )
  if (!fs.existsSync(tarball)) throw new Error(`npm pack omitted the expected tarball: ${tarball}`)

  fs.writeFileSync(
    path.join(consumer, 'package.json'),
    `${JSON.stringify(
      {
        private: true,
        dependencies: {
          [packageManifest.name]: `file:${tarball}`
        }
      },
      null,
      2
    )}\n`
  )
  run(
    'npm',
    ['install', '--ignore-scripts', '--no-audit', '--no-fund', '--legacy-peer-deps'],
    { cwd: consumer }
  )

  const installedPackage = path.join(consumer, 'node_modules', packageManifest.name)
  if (!fs.existsSync(path.join(installedPackage, 'native', 'tauri', 'Cargo.toml'))) {
    throw new Error('packed npm artifact omitted native/tauri/Cargo.toml')
  }
  if (!fs.existsSync(path.join(installedPackage, 'vendor', 'btleplug', 'UBM_PATCHES.md'))) {
    throw new Error('packed npm artifact omitted the mandatory vendored btleplug patch set')
  }

  fs.writeFileSync(
    path.join(cargoRoot, 'Cargo.toml'),
    `[package]
name = "ubm-tauri-packed-consumer"
version = "0.0.0"
edition = "2021"
publish = false

[dependencies]
tauri-plugin-unified-ble-manager = { path = "../node_modules/unified-ble-manager/native/tauri" }

[patch.crates-io]
btleplug = { path = "../node_modules/unified-ble-manager/vendor/btleplug" }
bluez-async = { path = "../node_modules/unified-ble-manager/vendor/bluez-async" }
`
  )
  fs.writeFileSync(
    path.join(cargoRoot, 'src', 'lib.rs'),
    'pub use tauri_plugin_unified_ble_manager::{BtleplugDispatcher, PluginBuilder};\n'
  )

  const toolchain = fs
    .readFileSync(path.join(root, 'rust-toolchain.toml'), 'utf8')
    .match(/^channel\s*=\s*"([^"]+)"/m)?.[1]
  if (!toolchain) throw new Error('could not parse the pinned Rust toolchain')

  run(
    'rustup',
    ['run', toolchain, 'cargo', 'check', '--manifest-path', path.join(cargoRoot, 'Cargo.toml')],
    {
      cwd: consumer,
      env: { CARGO_TARGET_DIR: path.join(temporaryDirectory, 'cargo-target') }
    }
  )

  process.stdout.write(
    `Packed Tauri consumer proof passed for ${packageManifest.name}@${packageManifest.version}.\n`
  )
} finally {
  removeTemporaryDirectory(temporaryDirectory)
}
