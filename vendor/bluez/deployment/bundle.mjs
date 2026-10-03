// SPDX-License-Identifier: GPL-2.0-or-later
// Explicit source build only. No downloads, dependency installation or daemon launch.
import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { createHash } from 'node:crypto'
import { spawnSync } from 'node:child_process'
import identity from './identity.cjs'

export const digest = file => createHash('sha256').update(fs.readFileSync(file)).digest('hex')
const assetsDefault = fileURLToPath(new URL('../', import.meta.url))
const isDigest = value => typeof value === 'string' && /^[a-f0-9]{64}$/.test(value)
export function absolute(value) {
  if (typeof value !== 'string' || !path.isAbsolute(value) || path.resolve(value) === '/') {
    throw new Error('a non-root absolute path is required')
  }
  return path.resolve(value)
}
function fresh(value) {
  const resolved = absolute(value)
  if (fs.existsSync(resolved)) throw new Error(`destination exists: ${resolved}`)
  return resolved
}
function contained(root, candidate) {
  const relative = path.relative(root, candidate)
  return relative === '' || (!relative.startsWith(`..${path.sep}`) && relative !== '..' && !path.isAbsolute(relative))
}
function runTool(command, args, cwd) {
  const result = spawnSync(command, args, { cwd, encoding: 'utf8', maxBuffer: 32 * 1024 * 1024, shell: false })
  const output = `${result.stdout ?? ''}${result.stderr ?? ''}`
  if (result.error || result.status !== 0)
    throw new Error(`${command} failed: ${result.error?.message ?? result.status}\n${output}`)
  return output
}
function checkedAsset(assets, record) {
  if (!record || !/^[A-Za-z0-9_.-]+$/.test(record.file) || !isDigest(record.sha256)) {
    throw new Error('invalid source asset identity')
  }
  const file = path.join(assets, record.file)
  if (!fs.lstatSync(file).isFile() || digest(file) !== record.sha256)
    throw new Error(`source asset digest mismatch: ${record.file}`)
  return file
}
export function buildBundle(options, dependencies = {}) {
  if (!dependencies.run && process.platform !== 'linux') throw new Error('production builds require Linux')
  const output = fresh(options.output)
  const work = fresh(options.work)
  const archive = absolute(options.archive)
  if (contained(work, output) || contained(output, work) || contained(output, archive) || contained(work, archive)) {
    throw new Error('archive, bundle and working trees must be disjoint')
  }
  const assets = dependencies.assets ?? assetsDefault
  if (contained(assets, output) || contained(assets, work))
    throw new Error('outputs must be outside the source asset tree')
  const manifestFile = path.join(assets, 'source-asset-manifest.json')
  const manifestBytes = fs.readFileSync(manifestFile)
  const manifestSha256 = createHash('sha256').update(manifestBytes).digest('hex')
  const manifest = JSON.parse(manifestBytes.toString('utf8'))
  if (
    manifest.schemaVersion !== 1 ||
    manifest.name !== 'bluez' ||
    manifest.version !== '5.87' ||
    JSON.stringify(manifest.distribution?.linuxAuthorityContract) !== '[1,1,1]'
  ) {
    throw new Error('source producer is not ready for Linux authority deployment')
  }
  const release = identity.deploymentRelease(manifest.distribution)
  if (options.release !== undefined && options.release !== release)
    throw new Error('requested release differs from the source manifest')
  if (
    !isDigest(manifest.upstream?.sha256) ||
    !fs.lstatSync(archive).isFile() ||
    digest(archive) !== manifest.upstream.sha256
  ) {
    throw new Error('upstream archive digest mismatch')
  }
  checkedAsset(assets, manifest.patch)
  if (!Array.isArray(manifest.licenseFiles) || manifest.licenseFiles.length < 2)
    throw new Error('source licenses missing')
  manifest.licenseFiles.forEach(record => checkedAsset(assets, record))
  const prefix = `/opt/unified-ble-manager/bluez/${release}-${manifest.patch.sha256.slice(0, 12)}`
  const run = dependencies.run ?? runTool
  fs.mkdirSync(work, { mode: 0o700 })
  fs.mkdirSync(output, { mode: 0o700 })
  // Freeze all source assets before invoking any tool. Both preparation
  // calls consume these retained, checked bytes, never a changing checkout.
  inventory(assets)
  fs.cpSync(assets, output, { recursive: true })
  if (!fs.existsSync(path.join(output, 'deployment'))) {
    fs.cpSync(fileURLToPath(new URL('./', import.meta.url)), path.join(output, 'deployment'), { recursive: true })
  }
  fs.copyFileSync(archive, path.join(output, 'upstream.tar.xz'))
  const frozenArchive = path.join(output, 'upstream.tar.xz')
  if (
    digest(frozenArchive) !== manifest.upstream.sha256 ||
    digest(path.join(output, 'source-asset-manifest.json')) !== manifestSha256
  ) {
    throw new Error('input identity changed while freezing source')
  }
  checkedAsset(output, manifest.patch)
  manifest.licenseFiles.forEach(record => checkedAsset(output, record))
  const logs = []
  const invoke = (command, args, cwd) => {
    logs.push(JSON.stringify({ command, args, cwd }) + '\n')
    try {
      const output = run(command, args, cwd)
      logs.push(output)
      return output
    } catch (error) {
      logs.push(`${error instanceof Error ? error.message : String(error)}\n`)
      throw error
    } finally {
      fs.writeFileSync(path.join(output, 'build.log'), logs.join(''))
    }
  }
  // The isolated test producer and deployed producer never share configure state.
  const tested = path.join(work, 'test-source')
  const deployed = path.join(work, 'production-source')
  invoke('sh', [path.join(output, 'prepare-isolated.sh'), frozenArchive, tested], work)
  invoke('sh', [path.join(output, 'build-test-isolated.sh'), path.join(tested, 'bluez-5.87')], work)
  invoke('sh', [path.join(output, 'prepare-isolated.sh'), frozenArchive, deployed], work)
  const source = path.join(deployed, 'bluez-5.87')
  invoke('tar', ['-cJf', path.join(output, 'corresponding-source.tar.xz'), '-C', deployed, 'bluez-5.87'], work)
  const configure = productionConfigure(prefix)
  invoke(path.join(source, 'configure'), configure, source)
  invoke('make', ['-j1', 'src/builtin.h', 'src/bluetoothd'], source)
  fs.copyFileSync(path.join(source, 'src/bluetoothd'), path.join(output, 'bluetoothd'))
  const header = fs.readFileSync(path.join(output, 'bluetoothd'))
  const machine = { x64: 62, arm64: 183 }[process.arch]
  if (
    header.length < 64 ||
    !header.subarray(0, 6).equals(Buffer.from([0x7f, 0x45, 0x4c, 0x46, 2, 1])) ||
    machine === undefined ||
    header.readUInt16LE(18) !== machine
  )
    throw new Error('binary is not a maintained native Linux ELF target')
  fs.chmodSync(path.join(output, 'bluetoothd'), 0o755)
  fs.writeFileSync(
    path.join(output, 'elf-dynamic.txt'),
    invoke('readelf', ['-d', path.join(source, 'src/bluetoothd')], source) ?? ''
  )
  for (const file of ['config.h', 'config.log']) fs.copyFileSync(path.join(source, file), path.join(output, file))
  const environment = Object.fromEntries(
    ['CC', 'CFLAGS', 'CPPFLAGS', 'LDFLAGS', 'PKG_CONFIG_PATH', 'UDEV_CFLAGS', 'UDEV_LIBS', 'PATH'].map(key => [
      key,
      process.env[key] ?? null
    ])
  )
  fs.writeFileSync(
    path.join(output, 'build-settings.json'),
    JSON.stringify(
      {
        configure,
        prefix,
        environment,
        sourceArchiveSha256: manifest.upstream.sha256,
        producerGate: { outcome: 'passed', sha256: digest(path.join(output, 'build-test-isolated.sh')) },
        preparationSha256: digest(path.join(output, 'prepare-isolated.sh'))
      },
      null,
      2
    ) + '\n'
  )
  const files = inventory(output)
  const bundle = {
    schema: 'ubm-bluez-deployment/1',
    release,
    prefix,
    authorityContract: [1, 1, 1],
    qualification: 'built-not-radio-qualified',
    platform: process.platform,
    architecture: process.arch,
    sourceManifestSha256: manifestSha256,
    files
  }
  fs.writeFileSync(path.join(output, 'bundle.json'), JSON.stringify(bundle, null, 2) + '\n')
  return verifyBundle(output)
}
function productionConfigure(prefix) {
  return [
    `--prefix=${prefix}`,
    `--libexecdir=${prefix}/libexec`,
    '--sysconfdir=/etc',
    '--localstatedir=/var',
    '--with-udevdir=/usr/lib/udev',
    '--with-systemdsystemunitdir=/usr/lib/systemd/system',
    '--with-systemduserunitdir=/usr/lib/systemd/user',
    '--disable-dependency-tracking',
    '--disable-client',
    '--disable-tools',
    '--disable-monitor',
    '--disable-cups',
    '--disable-obex',
    '--disable-manpages'
  ]
}
function inventory(root, directory = root) {
  return fs
    .readdirSync(directory)
    .sort()
    .flatMap(name => {
      const file = path.join(directory, name)
      const stat = fs.lstatSync(file)
      if (stat.isDirectory()) return inventory(root, file)
      if (!stat.isFile()) throw new Error('source/bundle entries must be regular files or directories')
      return [{ path: path.relative(root, file).split(path.sep).join('/'), sha256: digest(file) }]
    })
}
function safeRelative(value) {
  return (
    typeof value === 'string' &&
    value.split('/').every(part => /^[A-Za-z0-9_.-]+$/.test(part) && part !== '.' && part !== '..')
  )
}
export function verifyBundle(directory) {
  const root = absolute(directory)
  const bundle = JSON.parse(fs.readFileSync(path.join(root, 'bundle.json'), 'utf8'))
  if (
    bundle.schema !== 'ubm-bluez-deployment/1' ||
    JSON.stringify(bundle.authorityContract) !== '[1,1,1]' ||
    !/^\/opt\/unified-ble-manager\/bluez\/5\.87-ubm\.[1-9][0-9]*-[a-f0-9]{12}$/.test(bundle.prefix) ||
    !Array.isArray(bundle.files)
  )
    throw new Error('malformed deployment bundle')
  const names = new Set()
  for (const file of bundle.files) {
    if (!safeRelative(file.path) || names.has(file.path) || !isDigest(file.sha256))
      throw new Error('invalid bundle file identity')
    names.add(file.path)
    const target = path.join(root, file.path)
    if (!fs.lstatSync(target).isFile() || digest(target) !== file.sha256)
      throw new Error(`bundle digest mismatch: ${file.path}`)
  }
  for (const required of [
    'bluetoothd',
    'corresponding-source.tar.xz',
    'upstream.tar.xz',
    'source-asset-manifest.json',
    'COPYING',
    'COPYING.LIB',
    'build-settings.json',
    'build.log',
    'config.h',
    'config.log',
    'elf-dynamic.txt',
    'prepare-isolated.sh',
    'build-test-isolated.sh',
    'deployment/bundle.mjs',
    'deployment/identity.cjs',
    'deployment/activate.mjs'
  ]) {
    if (!names.has(required)) throw new Error(`bundle omits ${required}`)
  }
  const source = JSON.parse(fs.readFileSync(path.join(root, 'source-asset-manifest.json'), 'utf8'))
  if (
    digest(path.join(root, 'source-asset-manifest.json')) !== bundle.sourceManifestSha256 ||
    JSON.stringify(source.distribution?.linuxAuthorityContract) !== '[1,1,1]' ||
    source.distribution.release !== bundle.release
  )
    throw new Error('source manifest identity mismatch')
  checkedAsset(root, source.patch)
  if (
    bundle.prefix !== `/opt/unified-ble-manager/bluez/${bundle.release}-${source.patch.sha256.slice(0, 12)}` ||
    digest(path.join(root, 'upstream.tar.xz')) !== source.upstream.sha256
  )
    throw new Error('release/source prefix identity mismatch')
  const settings = JSON.parse(fs.readFileSync(path.join(root, 'build-settings.json'), 'utf8'))
  if (
    settings.prefix !== bundle.prefix ||
    JSON.stringify(settings.configure) !== JSON.stringify(productionConfigure(bundle.prefix))
  ) {
    throw new Error('production build settings mismatch')
  }
  if (
    settings.producerGate?.outcome !== 'passed' ||
    settings.producerGate.sha256 !== digest(path.join(root, 'build-test-isolated.sh')) ||
    settings.preparationSha256 !== digest(path.join(root, 'prepare-isolated.sh'))
  )
    throw new Error('producer test/preparation identity mismatch')
  for (const license of source.licenseFiles) checkedAsset(root, license)
  return bundle
}
if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const [archive, output, work, release, ...extra] = process.argv.slice(2)
  if (!archive || !output || !work || extra.length)
    throw new Error('usage: node bundle.mjs /absolute/archive /absolute/new-bundle /absolute/new-work [5.87-ubm.N]')
  const result = buildBundle({ archive, output, work, release })
  console.log(JSON.stringify(result, null, 2))
}
