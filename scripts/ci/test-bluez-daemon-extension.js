// Source-only Linux regression gate. Never installs or launches bluetoothd.
const { execFileSync } = require('node:child_process')
const crypto = require('node:crypto')
const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')
const { pathToFileURL } = require('node:url')
const { readBluezSourceAsset } = require('../release/bluez-source-asset')

async function run(argv = process.argv.slice(2)) {
  if (process.platform !== 'linux') throw new Error('BlueZ daemon-source tests require Linux')
  if (argv.length !== 0 && (argv.length !== 2 || argv[0] !== '--archive' || !path.isAbsolute(argv[1]))) {
    throw new Error('usage: node scripts/ci/test-bluez-daemon-extension.js [--archive /absolute/bluez-5.87.tar.xz]')
  }
  const assets = path.resolve(__dirname, '../../vendor/bluez')
  const manifest = readBluezSourceAsset(assets)
  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-bluez-daemon-test-'))
  let completed = false
  try {
    const archive = argv.length === 2 ? argv[1] : path.join(scratch, manifest.upstream.archive)
    if (argv.length === 0) {
      execFileSync(
        'curl',
        [
          '--fail',
          '--location',
          '--proto',
          '=https',
          '--tlsv1.2',
          '--connect-timeout',
          '30',
          '--max-time',
          '180',
          '--retry',
          '2',
          '--output',
          archive,
          manifest.upstream.url
        ],
        { stdio: 'inherit' }
      )
    }
    const actual = crypto.createHash('sha256').update(fs.readFileSync(archive)).digest('hex')
    if (actual !== manifest.upstream.sha256) throw new Error('Refusing mismatched upstream BlueZ archive')
    const { buildBundle } = await import(pathToFileURL(path.join(assets, 'deployment/bundle.mjs')).href)
    const receipt = buildBundle({ archive, output: path.join(scratch, 'bundle'), work: path.join(scratch, 'work') })
    console.log(JSON.stringify({ release: receipt.release, qualification: receipt.qualification }))
    completed = true
  } finally {
    if (completed) fs.rmSync(scratch, { recursive: true, force: true })
    else console.error(`BlueZ deployment gate failed; diagnostics retained at ${scratch}`)
  }
}

if (require.main === module) {
  run().catch(error => {
    console.error(error)
    process.exitCode = 1
  })
}
module.exports = { run }
