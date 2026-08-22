'use strict'

const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')
const { spawnSync } = require('node:child_process')

const root = path.resolve(__dirname, '../..')
const trackedHtmlPath = path.join(root, 'docs/index.html')
const forbiddenPublicApiTokens = Object.freeze([
  'unified-ble-manager/advanced',
  'unified-ble-manager/backend-sdk',
  'unified-ble-manager/react-native',
  'unified-ble-manager/web',
  'unified-ble-manager/node/',
  'unified-ble-manager/electron/',
  'unified-ble-manager/tauri',
  'unified-ble-manager/expo',
  'ApplicationBleManager',
  'CoreBoundedStream',
  'ElectronRendererBleClient',
  'IpcBleManager',
  'TauriBleManager',
  'createBackendOperationCapabilityRegistration',
  'normalizeOperationOptions'
])

function extractPublicApiSection(html) {
  const headingStart = html.indexOf('<h2 id="public-api"')
  if (headingStart === -1) throw new Error('Generated HTML is missing the Public API section')
  const sectionEnd = html.indexOf('</section>', headingStart)
  if (sectionEnd === -1) throw new Error('Generated HTML has an unterminated Public API section')
  return html.slice(headingStart, sectionEnd)
}

function assertPublicApiBoundary(html) {
  const publicApi = extractPublicApiSection(html)
  for (const token of forbiddenPublicApiTokens) {
    if (publicApi.includes(token)) {
      throw new Error(`Public API section advertises forbidden symbol or entrypoint: ${token}`)
    }
  }
}

function assertByteIdentical(committed, regenerated) {
  const committedBuffer = Buffer.isBuffer(committed) ? committed : Buffer.from(committed)
  const regeneratedBuffer = Buffer.isBuffer(regenerated) ? regenerated : Buffer.from(regenerated)
  if (!committedBuffer.equals(regeneratedBuffer)) {
    throw new Error('Generated HTML differs byte-for-byte from docs/index.html')
  }
}

function run(command, args) {
  const result = spawnSync(command, args, { cwd: root, encoding: 'utf8', stdio: 'inherit' })
  if (result.error) throw result.error
  if (result.status !== 0) throw new Error(`${command} ${args.join(' ')} failed with exit code ${String(result.status)}`)
}

function checkGeneratedHtml() {
  const input = path.join(root, 'lib/module/index.js')
  if (!fs.existsSync(input)) throw new Error('Generated documentation input is missing; run the package build first')
  const temporaryRoot = fs.mkdtempSync(path.join(os.tmpdir(), 'unified-ble-manager-docs-'))
  const temporaryOutput = path.join(temporaryRoot, 'docs')
  try {
    run('pnpm', ['exec', 'documentation', 'build', 'lib/module/index.js', '-o', temporaryOutput, '--config', 'documentation.yml', '-f', 'html', '--shallow'])
    run('pnpm', [
      'exec',
      'prettier',
      '--config',
      path.join(root, '.prettierrc'),
      '--write',
      path.join(temporaryOutput, 'index.html'),
      path.join(temporaryOutput, 'assets/anchor.js'),
      path.join(temporaryOutput, 'assets/site.js')
    ])
    const regenerated = fs.readFileSync(path.join(temporaryOutput, 'index.html'))
    assertByteIdentical(fs.readFileSync(trackedHtmlPath), regenerated)
    assertPublicApiBoundary(regenerated.toString('utf8'))
    console.log('Generated HTML checked: byte-identical and root-public-safe')
  } finally {
    fs.rmSync(temporaryRoot, { recursive: true, force: true })
  }
}

if (require.main === module) checkGeneratedHtml()

module.exports = { assertByteIdentical, assertPublicApiBoundary, extractPublicApiSection, checkGeneratedHtml }
