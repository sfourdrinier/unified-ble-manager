// SPDX-License-Identifier: GPL-2.0-or-later
// Explicit root file installation only. Does not reload/restart/launch any service.
import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { absolute, digest, verifyBundle } from './bundle.mjs'

const prefixPattern = /^\/opt\/unified-ble-manager\/bluez\/5\.87-ubm\.[1-9][0-9]*-[a-f0-9]{12}$/
const override = '/etc/systemd/system/bluetooth.service.d/90-ubm-authority.conf'
export function prepareOverride(prefix, unit, argv, allowExperimental) {
  if (!prefixPattern.test(prefix)) throw new Error('invalid versioned installation prefix')
  if (!allowExperimental) throw new Error('experimental D-Bus enablement needs explicit approval')
  if (
    !Array.isArray(argv) ||
    argv.length === 0 ||
    !argv.every(arg => typeof arg === 'string' && /^[A-Za-z0-9_./=,:+-]+$/.test(arg)) ||
    !path.isAbsolute(argv[0]) ||
    path.basename(argv[0]) !== 'bluetoothd'
  )
    throw new Error('unsupported service argument')
  // Deliberately refuse complex quoting/environment expansions; operators
  // must supply a separately reviewed deployment for those host unit forms.
  const starts = unit.split(/\r?\n/).filter(line => line.startsWith('ExecStart=') && line !== 'ExecStart=')
  if (starts.length !== 1 || starts[0] !== `ExecStart=${argv.join(' ')}`)
    throw new Error('reviewed ExecStart does not match the supplied unit snapshot')
  const args = argv.slice(1)
  if (!args.includes('--experimental')) args.push('--experimental')
  // Do not reset or amend capability/sandbox/restart/storage directives.
  return `[Service]\nExecStart=\nExecStart=${prefix}/libexec/bluetoothd ${args.join(' ')}\n`
}
function authorized(options, dependencies) {
  if (options.confirm !== true) throw new Error('explicit file-install or rollback confirmation is required')
  if ((dependencies.uid ?? (() => process.geteuid?.()))() !== 0)
    throw new Error('root is required; this owner never escalates')
}
function secureDirectory(directory, create = false) {
  if (directory === '/') return
  secureDirectory(path.dirname(directory), create)
  if (!fs.existsSync(directory)) {
    if (!create) throw new Error(`missing root directory: ${directory}`)
    fs.mkdirSync(directory, { mode: 0o755 })
  }
  const stat = fs.lstatSync(directory)
  if (!stat.isDirectory() || stat.uid !== 0 || (stat.mode & 0o022) !== 0)
    throw new Error(`directory must be root-owned and non-user-writable: ${directory}`)
}
function exclusiveFile(file, bytes, mode) {
  const fd = fs.openSync(file, 'wx', mode)
  try {
    fs.writeFileSync(fd, bytes)
    fs.fsyncSync(fd)
  } finally {
    fs.closeSync(fd)
  }
}
export function installBundle(options, dependencies = {}) {
  authorized(options, dependencies)
  if (process.platform !== 'linux') throw new Error('system installation requires Linux')
  const bundleRoot = absolute(options.bundle)
  const bundle = verifyBundle(bundleRoot)
  if (bundle.platform !== 'linux' || bundle.architecture !== process.arch)
    throw new Error('bundle target is not this Linux host')
  const unit = fs.readFileSync(absolute(options.unit), 'utf8')
  const argv = JSON.parse(fs.readFileSync(absolute(options.argv), 'utf8'))
  const dropin = prepareOverride(bundle.prefix, unit, argv, options.allowExperimental)
  if (fs.existsSync(bundle.prefix) || fs.existsSync(override))
    throw new Error('versioned prefix or UBM override already exists; nothing overwritten')
  secureDirectory(path.dirname(bundle.prefix), true)
  secureDirectory(path.dirname(override), true)
  fs.mkdirSync(bundle.prefix, { mode: 0o755 })
  fs.mkdirSync(path.join(bundle.prefix, 'libexec'), { mode: 0o755 })
  fs.mkdirSync(path.join(bundle.prefix, 'corresponding-source'), { mode: 0o755 })
  for (const file of [...bundle.files, { path: 'bundle.json' }]) {
    const target =
      file.path === 'bluetoothd'
        ? path.join(bundle.prefix, 'libexec/bluetoothd')
        : path.join(bundle.prefix, 'corresponding-source', file.path)
    fs.mkdirSync(path.dirname(target), { recursive: true, mode: 0o755 })
    exclusiveFile(target, fs.readFileSync(path.join(bundleRoot, file.path)), file.path === 'bluetoothd' ? 0o755 : 0o644)
    if (file.sha256 && digest(target) !== file.sha256)
      throw new Error(`installed digest mismatch: ${file.path}; incomplete prefix retained`)
  }
  exclusiveFile(path.join(bundle.prefix, 'original-service.txt'), unit, 0o644)
  exclusiveFile(path.join(bundle.prefix, 'original-exec-start.json'), JSON.stringify(argv) + '\n', 0o644)
  const receipt = {
    schema: 'ubm-bluez-install/1',
    prefix: bundle.prefix,
    override,
    overrideSha256: null,
    originalUnitSha256: digest(path.join(bundle.prefix, 'original-service.txt')),
    state: 'files-installed-service-unchanged'
  }
  // Receipt exists before exposing the override. Failed installation leaves
  // a visible incomplete prefix; it never overwrites a previous deployment.
  exclusiveFile(path.join(bundle.prefix, 'pending-override.conf'), dropin, 0o644)
  receipt.overrideSha256 = digest(path.join(bundle.prefix, 'pending-override.conf'))
  exclusiveFile(path.join(bundle.prefix, 'deployment-receipt.json'), JSON.stringify(receipt, null, 2) + '\n', 0o644)
  exclusiveFile(override, dropin, 0o644)
  return receipt
}
export function rollbackBundle(options, dependencies = {}) {
  authorized(options, dependencies)
  if (process.platform !== 'linux') throw new Error('system rollback requires Linux')
  const receiptPath = absolute(options.receipt)
  const receiptStat = fs.lstatSync(receiptPath)
  if (!receiptStat.isFile() || receiptStat.uid !== 0 || (receiptStat.mode & 0o022) !== 0)
    throw new Error('rollback receipt must be a root-owned regular file')
  const receipt = JSON.parse(fs.readFileSync(receiptPath, 'utf8'))
  if (
    receipt.schema !== 'ubm-bluez-install/1' ||
    !prefixPattern.test(receipt.prefix) ||
    receipt.override !== override ||
    receiptPath !== path.join(receipt.prefix, 'deployment-receipt.json')
  )
    throw new Error('invalid scoped rollback receipt')
  secureDirectory(receipt.prefix)
  secureDirectory(path.dirname(override))
  const stat = fs.lstatSync(override)
  if (!stat.isFile() || stat.uid !== 0 || (stat.mode & 0o022) !== 0 || digest(override) !== receipt.overrideSha256) {
    throw new Error('override changed; refusing to remove another owner configuration')
  }
  fs.unlinkSync(override)
  return { state: 'override-removed-service-unchanged', retainedPrefix: receipt.prefix }
}
if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const [command, ...args] = process.argv.slice(2)
  let result
  if (
    command === 'install' &&
    args.length === 5 &&
    args[3] === '--allow-experimental' &&
    args[4] === '--confirm-file-install'
  ) {
    result = installBundle({ bundle: args[0], unit: args[1], argv: args[2], allowExperimental: true, confirm: true })
  } else if (command === 'rollback' && args.length === 2 && args[1] === '--confirm-override-removal') {
    result = rollbackBundle({ receipt: args[0], confirm: true })
  } else
    throw new Error(
      'usage: activate.mjs install BUNDLE UNIT_SNAPSHOT EXEC_ARGV_JSON --allow-experimental --confirm-file-install | rollback RECEIPT --confirm-override-removal'
    )
  console.log(JSON.stringify(result, null, 2))
}
