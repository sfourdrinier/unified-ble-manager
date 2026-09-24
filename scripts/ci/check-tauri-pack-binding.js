#!/usr/bin/env node

'use strict'

const crypto = require('crypto')
const fs = require('fs')
const path = require('path')

function argument(name) {
  const index = process.argv.indexOf(name)
  if (index === -1 || !process.argv[index + 1]) throw new Error(`${name} requires a path`)
  return path.resolve(process.argv[index + 1])
}

const receipt = JSON.parse(fs.readFileSync(argument('--receipt'), 'utf8'))
const tarball = argument('--tarball')
const manifest = require('../../package.json')
const expectedPackage = `${manifest.name}@${manifest.version}`
const actualSha256 = crypto.createHash('sha256').update(fs.readFileSync(tarball)).digest('hex')
if (receipt.package !== expectedPackage || receipt.proof !== 'linked-tauri-application') {
  throw new Error('Tauri proof receipt does not identify this package and linked application build')
}
if (receipt.tarballSha256 !== actualSha256) {
  throw new Error(`Tauri linked application used different bytes: ${receipt.tarballSha256} != ${actualSha256}`)
}
process.stdout.write(`Linked Tauri application consumed the exact publish tarball (SHA-256 ${actualSha256}).\n`)
