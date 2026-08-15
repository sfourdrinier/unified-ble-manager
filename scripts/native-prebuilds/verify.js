#!/usr/bin/env node
'use strict'

const crypto = require('crypto')
const fs = require('fs')
const path = require('path')
const { NODE_API_VERSION, NATIVE_PREBUILD_TARGETS } = require('./targets')

const root = path.resolve(__dirname, '../..')

function listNativeBinaries(directory) {
  if (!fs.existsSync(directory)) return []
  const files = []
  for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
    const entryPath = path.join(directory, entry.name)
    if (entry.isDirectory()) files.push(...listNativeBinaries(entryPath))
    else if (entry.isFile() && entry.name.endsWith('.node')) files.push(entryPath)
  }
  return files
}

function sha256(filePath) {
  return crypto.createHash('sha256').update(fs.readFileSync(filePath)).digest('hex')
}

function main(argv) {
  const allowedArguments = new Set(['--require-all', '--write-manifest'])
  for (const argument of argv) {
    if (!allowedArguments.has(argument)) throw new Error(`Unknown native-prebuild verification argument: ${argument}`)
  }
  const requireAll = argv.includes('--require-all')
  const writeManifest = argv.includes('--write-manifest')
  if (writeManifest && !requireAll) {
    throw new Error('--write-manifest requires --require-all')
  }

  const expectedPaths = new Set(NATIVE_PREBUILD_TARGETS.map(target => target.prebuildPath))
  const actualPaths = listNativeBinaries(path.join(root, 'native'))
    .map(filePath => path.relative(root, filePath).split(path.sep).join('/'))
    .sort()
  const unexpected = actualPaths.filter(filePath => !expectedPaths.has(filePath))
  if (unexpected.length > 0) {
    throw new Error(`Unexpected native prebuilds: ${unexpected.join(', ')}`)
  }

  const missing = NATIVE_PREBUILD_TARGETS.filter(target => !actualPaths.includes(target.prebuildPath))
  if (requireAll && missing.length > 0) {
    throw new Error(`Missing required native prebuilds: ${missing.map(target => target.prebuildPath).join(', ')}`)
  }

  const entries = NATIVE_PREBUILD_TARGETS.filter(target => actualPaths.includes(target.prebuildPath)).map(target => {
    const filePath = path.join(root, ...target.prebuildPath.split('/'))
    const size = fs.statSync(filePath).size
    if (size === 0) throw new Error(`Native prebuild is empty: ${target.prebuildPath}`)
    return Object.freeze({
      backend: target.backend,
      platform: target.platform,
      arch: target.arch,
      nodeApiVersion: NODE_API_VERSION,
      path: target.prebuildPath,
      bytes: size,
      sha256: sha256(filePath)
    })
  })

  if (writeManifest) {
    const packageJson = require(path.join(root, 'package.json'))
    const manifest = {
      schemaVersion: 1,
      package: `${packageJson.name}@${packageJson.version}`,
      nodeApiVersion: NODE_API_VERSION,
      entries
    }
    fs.writeFileSync(path.join(root, 'native', 'PREBUILDS.json'), `${JSON.stringify(manifest, null, 2)}\n`)
  }

  process.stdout.write(
    `${JSON.stringify({ verified: entries.length, required: NATIVE_PREBUILD_TARGETS.length, complete: missing.length === 0 })}\n`
  )
}

main(process.argv.slice(2))
