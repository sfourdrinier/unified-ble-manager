#!/usr/bin/env node

'use strict'

const fs = require('fs')
const crypto = require('crypto')
const os = require('os')
const path = require('path')
const { spawnSync } = require('child_process')

const root = path.resolve(__dirname, '../..')
const packageManifest = require(path.join(root, 'package.json'))
const timeoutMs = 15 * 60 * 1000
const receiptIndex = process.argv.indexOf('--receipt')
const receiptPath = receiptIndex === -1 ? null : process.argv[receiptIndex + 1]
if (receiptIndex !== -1 && !receiptPath) throw new Error('--receipt requires a path')

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
  fs.mkdirSync(path.join(consumer, 'frontend'))
  fs.writeFileSync(path.join(consumer, 'frontend', 'index.html'), '<!doctype html><title>UBM packed Tauri proof</title>\n')

  run('npm', ['pack', '--ignore-scripts', '--pack-destination', artifacts])
  const tarball = path.join(
    artifacts,
    `${packageManifest.name.replace(/^@/, '').replace('/', '-')}-${packageManifest.version}.tgz`
  )
  if (!fs.existsSync(tarball)) throw new Error(`npm pack omitted the expected tarball: ${tarball}`)
  const tarballSha256 = crypto.createHash('sha256').update(fs.readFileSync(tarball)).digest('hex')

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

  const { tauriCargoRecipe } = require(path.join(root, 'lib', 'commonjs', 'tauri', 'install-recipe.js'))
  fs.writeFileSync(path.join(cargoRoot, 'Cargo.toml'), `[package]
name = "ubm-tauri-packed-consumer"
version = "0.0.0"
edition = "2021"
publish = false

[build-dependencies]
tauri-build = { version = "2", features = [] }

${tauriCargoRecipe()}`)
  fs.writeFileSync(
    path.join(cargoRoot, 'build.rs'),
    'fn main() { tauri_build::build() }\n'
  )
  fs.writeFileSync(
    path.join(cargoRoot, 'src', 'main.rs'),
    `fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_unified_ble_manager::PluginBuilder::new(
            tauri_plugin_unified_ble_manager::BtleplugDispatcher::default(),
        ).build())
        .run(tauri::generate_context!())
        .expect("packed Tauri application failed");
}
`
  )
  fs.writeFileSync(path.join(cargoRoot, 'tauri.conf.json'), JSON.stringify({
    productName: 'UBM Packed Consumer',
    version: '0.0.0',
    identifier: 'com.unifiedblemanager.packedconsumer',
    build: { frontendDist: '../frontend' },
    app: { windows: [{ label: 'main', title: 'UBM Packed Consumer' }] },
    bundle: { active: false }
  }, null, 2))
  fs.mkdirSync(path.join(cargoRoot, 'icons'))
  fs.copyFileSync(
    path.join(root, 'assets/brand/ubm-mark-512.png'),
    path.join(cargoRoot, 'icons/icon.png')
  )
  fs.mkdirSync(path.join(cargoRoot, 'capabilities'))
  fs.writeFileSync(path.join(cargoRoot, 'capabilities', 'main.json'), JSON.stringify({
    identifier: 'main',
    windows: ['main'],
    permissions: ['core:default', 'unified-ble-manager:default']
  }, null, 2))

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
  run(
    'rustup',
    ['run', toolchain, 'cargo', 'build', '--manifest-path', path.join(cargoRoot, 'Cargo.toml')],
    {
      cwd: consumer,
      env: { CARGO_TARGET_DIR: path.join(temporaryDirectory, 'cargo-target') }
    }
  )

  if (receiptPath !== null) {
    fs.writeFileSync(path.resolve(root, receiptPath), `${JSON.stringify({
      package: `${packageManifest.name}@${packageManifest.version}`,
      tarballSha256,
      proof: 'linked-tauri-application'
    }, null, 2)}\n`)
  }

  process.stdout.write(
    `Packed Tauri dependency check and linked application build passed for ${packageManifest.name}@${packageManifest.version} (SHA-256 ${tarballSha256}).\n`
  )
} finally {
  removeTemporaryDirectory(temporaryDirectory)
}
