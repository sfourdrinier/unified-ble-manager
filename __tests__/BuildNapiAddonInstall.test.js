'use strict'

// A rebuilt addon must replace the staged file with a fresh inode. Copying
// over a Mach-O image a running process has mapped invalidates its cached
// code signature on macOS and kills that process; renaming a fully written
// temporary file over the destination leaves the old image intact.

const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')

const {
  installAddon,
  resolveCargoTargetRoot,
  resolvePinnedRustc
} = require('../scripts/ci/build-napi-addon.js')

describe('build-napi-addon install step', () => {
  let directory

  beforeEach(() => {
    directory = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-addon-install-'))
  })

  afterEach(() => {
    fs.rmSync(directory, { recursive: true, force: true })
  })

  test('replaces an existing addon with a new inode instead of writing over it', () => {
    const built = path.join(directory, 'built.node')
    const destination = path.join(directory, 'staged', 'ubm_echo.node')
    fs.mkdirSync(path.dirname(destination), { recursive: true })
    fs.writeFileSync(destination, 'old image')
    const previousInode = fs.statSync(destination).ino
    fs.writeFileSync(built, 'new image')

    installAddon(built, destination)

    expect(fs.readFileSync(destination, 'utf8')).toBe('new image')
    expect(fs.statSync(destination).ino).not.toBe(previousInode)
    expect(fs.readdirSync(path.dirname(destination))).toEqual(['ubm_echo.node'])
  })

  test('creates the destination directory for a first install', () => {
    const built = path.join(directory, 'built.node')
    const destination = path.join(directory, 'fresh', 'nested', 'ubm_echo.node')
    fs.writeFileSync(built, 'image')

    installAddon(built, destination)

    expect(fs.readFileSync(destination, 'utf8')).toBe('image')
  })

  test('reads the built addon from Cargo target-dir overrides', () => {
    const root = path.resolve(directory, 'repo')
    expect(resolveCargoTargetRoot(root, undefined)).toBe(path.join(root, 'target'))
    expect(resolveCargoTargetRoot(root, 'custom-target')).toBe(path.join(root, 'custom-target'))
    expect(resolveCargoTargetRoot(root, path.join(directory, 'cargo-target'))).toBe(
      path.join(directory, 'cargo-target')
    )
  })

  test('resolves rustc from the pinned rustup toolchain instead of host PATH', () => {
    const calls = []
    const resolved = resolvePinnedRustc('1.98.1', (command, args) => {
      calls.push({ command, args })
      return '/toolchains/1.98.1/bin/rustc\n'
    })

    expect(resolved).toBe('/toolchains/1.98.1/bin/rustc')
    expect(calls).toEqual([
      { command: 'rustup', args: ['which', '--toolchain', '1.98.1', 'rustc'] }
    ])
  })
})
