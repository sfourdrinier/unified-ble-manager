import assert from 'node:assert/strict'
import test from 'node:test'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { createHash } from 'node:crypto'
import { buildBundle, verifyBundle } from './bundle.mjs'
import { prepareOverride, installBundle, rollbackBundle } from './activate.mjs'

const hash = bytes => createHash('sha256').update(bytes).digest('hex')
function fixture(t) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-bluez-deployment-test-'))
  t.after(() => fs.rmSync(root, { recursive: true, force: true }))
  const assets = path.join(root, 'assets')
  fs.mkdirSync(assets)
  fs.mkdirSync(path.join(assets, 'helpers'))
  fs.writeFileSync(path.join(assets, 'helpers', 'gate-marker.c'), 'retained relative helper')
  const archive = path.join(root, 'bluez.tar.xz')
  fs.writeFileSync(archive, 'upstream fixture')
  for (const file of ['extension.patch', 'COPYING', 'COPYING.LIB', 'prepare-isolated.sh', 'build-test-isolated.sh']) {
    fs.writeFileSync(path.join(assets, file), file)
  }
  const manifest = {
    schemaVersion: 1,
    name: 'bluez',
    version: '5.87',
    upstream: { archive: 'bluez-5.87.tar.xz', sha256: hash(fs.readFileSync(archive)) },
    patch: { file: 'extension.patch', sha256: hash('extension.patch') },
    licenseFiles: ['COPYING', 'COPYING.LIB'].map(file => ({ file, sha256: hash(file) })),
    distribution: { linuxAuthorityContract: [1, 1, 1], release: '5.87-ubm.1' }
  }
  fs.writeFileSync(path.join(assets, 'source-asset-manifest.json'), JSON.stringify(manifest))
  const calls = []
  const run = (command, args, cwd) => {
    calls.push({ command, args, cwd })
    if (command === 'sh' && args[0].endsWith('prepare-isolated.sh')) {
      fs.mkdirSync(path.join(args[2], 'bluez-5.87', 'src'), { recursive: true })
      fs.writeFileSync(path.join(args[2], 'bluez-5.87', 'configure'), 'source')
    }
    if (command === 'tar') fs.writeFileSync(args[1], 'corresponding source archive')
    if (command.endsWith('/configure')) {
      fs.writeFileSync(path.join(cwd, 'config.h'), 'production configuration')
      fs.writeFileSync(path.join(cwd, 'config.log'), 'configure toolchain')
    }
    if (command === 'make') {
      const elf = Buffer.alloc(64)
      elf.set([0x7f, 0x45, 0x4c, 0x46, 2, 1])
      elf.writeUInt16LE(process.arch === 'arm64' ? 183 : 62, 18)
      fs.writeFileSync(path.join(cwd, 'src/bluetoothd'), elf)
    }
    return 'tool output\n'
  }
  return {
    root,
    assets,
    archive,
    manifest,
    calls,
    run,
    options: { archive, output: path.join(root, 'bundle'), work: path.join(root, 'work'), release: '5.87-ubm.1' }
  }
}

test('build invokes fresh preparation, producer gates, and separate production paths without install', t => {
  const f = fixture(t)
  const bundle = buildBundle(f.options, { assets: f.assets, run: f.run })
  assert.equal(bundle.schema, 'ubm-bluez-deployment/1')
  assert.equal(bundle.qualification, 'built-not-radio-qualified')
  assert.deepEqual(bundle.authorityContract, [1, 1, 1])
  const prepares = f.calls.filter(call => call.command === 'sh' && call.args[0].endsWith('prepare-isolated.sh'))
  assert.equal(prepares.length, 2)
  assert.notEqual(prepares[0].args[2], prepares[1].args[2])
  const configure = f.calls.find(call => call.command.endsWith('/configure'))
  assert.ok(configure.args.includes('--sysconfdir=/etc'))
  assert.ok(configure.args.includes('--localstatedir=/var'))
  assert.ok(configure.args.includes(`--prefix=${bundle.prefix}`))
  assert.ok(!f.calls.some(call => ['sudo', 'systemctl', 'apt', 'curl'].includes(call.command)))
  assert.ok(!f.calls.some(call => call.command === 'make' && call.args.includes('install')))
  assert.ok(bundle.files.some(file => file.path === 'corresponding-source.tar.xz'))
  assert.ok(bundle.files.some(file => file.path === 'upstream.tar.xz'))
  assert.ok(bundle.files.some(file => file.path === 'helpers/gate-marker.c'))
  assert.ok(bundle.files.some(file => file.path === 'deployment/bundle.mjs'))
  assert.ok(bundle.files.some(file => file.path === 'deployment/activate.mjs'))
  verifyBundle(f.options.output)
  fs.appendFileSync(path.join(f.options.output, 'bluetoothd'), 'mutation')
  assert.throws(() => verifyBundle(f.options.output), /digest/)
})

test('unready producer, changed input, relative paths and existing destinations fail before tools', t => {
  const f = fixture(t)
  delete f.manifest.distribution.linuxAuthorityContract
  fs.writeFileSync(path.join(f.assets, 'source-asset-manifest.json'), JSON.stringify(f.manifest))
  assert.throws(() => buildBundle(f.options, { assets: f.assets, run: f.run }), /not ready/)
  f.manifest.distribution.linuxAuthorityContract = [1, 1, 1]
  fs.writeFileSync(path.join(f.assets, 'source-asset-manifest.json'), JSON.stringify(f.manifest))
  fs.appendFileSync(f.archive, 'mutation')
  assert.throws(() => buildBundle(f.options, { assets: f.assets, run: f.run }), /archive digest/)
  assert.throws(() => buildBundle({ ...f.options, output: 'relative' }, { assets: f.assets, run: f.run }), /absolute/)
  fs.mkdirSync(f.options.output)
  assert.throws(() => buildBundle(f.options, { assets: f.assets, run: f.run }), /exists/)
  assert.equal(f.calls.length, 0)
})

test('retained production settings include caller supplied dependency and tool search paths', t => {
  const f = fixture(t)
  const values = { UDEV_CFLAGS: '-I/isolated/headers', UDEV_LIBS: '/usr/lib/libudev.so.1' }
  for (const [key, value] of Object.entries(values)) {
    const previous = process.env[key]
    t.after(() => {
      if (previous === undefined) delete process.env[key]
      else process.env[key] = previous
    })
    process.env[key] = value
  }
  buildBundle(f.options, { assets: f.assets, run: f.run })
  const settings = JSON.parse(fs.readFileSync(path.join(f.options.output, 'build-settings.json'), 'utf8'))
  for (const [key, value] of Object.entries(values)) assert.equal(settings.environment[key], value)
  assert.equal(settings.environment.PATH, process.env.PATH ?? null)
})

test('the source manifest owns the deployment release and explicit mismatches fail before tools', t => {
  const f = fixture(t)
  assert.throws(
    () => buildBundle({ ...f.options, release: '5.87-ubm.2' }, { assets: f.assets, run: f.run }),
    /release.*manifest/
  )
  assert.equal(f.calls.length, 0)
  const { release, ...options } = f.options
  assert.equal(buildBundle(options, { assets: f.assets, run: f.run }).release, release)
})

test('override preserves current service arguments and leaves hardening untouched', () => {
  const unit =
    '[Service]\nExecStart=/usr/lib/bluetooth/bluetoothd --noplugin=hostname\nProtectSystem=strict\nCapabilityBoundingSet=CAP_NET_ADMIN CAP_NET_BIND_SERVICE\n'
  const argv = ['/usr/lib/bluetooth/bluetoothd', '--noplugin=hostname']
  const text = prepareOverride('/opt/unified-ble-manager/bluez/5.87-ubm.1-abcdef123456', unit, argv, true)
  assert.equal(
    text,
    '[Service]\nExecStart=\nExecStart=/opt/unified-ble-manager/bluez/5.87-ubm.1-abcdef123456/libexec/bluetoothd --noplugin=hostname --experimental\n'
  )
  assert.throws(() => prepareOverride('/tmp/user-writable', unit, argv, true), /prefix/)
  assert.throws(
    () => prepareOverride('/opt/unified-ble-manager/bluez/5.87-ubm.1-abcdef123456', unit, argv, false),
    /experimental/
  )
  assert.throws(
    () =>
      prepareOverride('/opt/unified-ble-manager/bluez/5.87-ubm.1-abcdef123456', unit, [argv[0], '$INJECTION'], true),
    /argument/
  )
})

test('a failed producer gate retains diagnostics and cannot produce an installable bundle', t => {
  const f = fixture(t)
  const run = (command, args, cwd) => {
    if (command === 'sh' && args[0].endsWith('build-test-isolated.sh')) throw new Error('lease-handler regression')
    return f.run(command, args, cwd)
  }
  assert.throws(() => buildBundle(f.options, { assets: f.assets, run }), /lease-handler regression/)
  assert.match(fs.readFileSync(path.join(f.options.output, 'build.log'), 'utf8'), /lease-handler regression/)
  assert.equal(fs.existsSync(path.join(f.options.output, 'bundle.json')), false)
  assert.equal(f.calls.filter(call => call.command.endsWith('/configure')).length, 0)
  assert.equal(f.calls.filter(call => call.command === 'make').length, 0)
})

test('privileged install and rollback refuse implicit authority and never run service commands', t => {
  const f = fixture(t)
  buildBundle(f.options, { assets: f.assets, run: f.run })
  const unit = path.join(f.root, 'service.txt')
  const argv = path.join(f.root, 'argv.json')
  fs.writeFileSync(unit, '[Service]\nExecStart=/usr/lib/bluetooth/bluetoothd\nProtectSystem=strict\n')
  fs.writeFileSync(argv, JSON.stringify(['/usr/lib/bluetooth/bluetoothd']))
  const options = { bundle: f.options.output, unit, argv, allowExperimental: true, confirm: false }
  assert.throws(() => installBundle(options), /explicit/)
  assert.throws(() => installBundle({ ...options, confirm: true }, { uid: () => 1000 }), /root/)
  assert.throws(() => rollbackBundle({ receipt: '/tmp/absent', confirm: false }), /explicit/)
})
