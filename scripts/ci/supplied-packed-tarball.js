'use strict'

const fs = require('node:fs')
const path = require('node:path')
const crypto = require('node:crypto')

// Opt-in only. The production publisher retains each helper's normal pack path.
function suppliedPackedTarball(environment = process.env) {
  const file = environment.UBM_PACKED_TARBALL
  const digest = environment.UBM_PACKED_TARBALL_SHA256
  if (file === undefined && digest === undefined) return null
  if (typeof file !== 'string' || file.length === 0) throw new Error('Supplied tarball requires a path')
  if (!path.isAbsolute(file)) throw new Error('Supplied tarball path must be absolute')
  if (typeof digest !== 'string' || !/^[a-f0-9]{64}$/.test(digest)) {
    throw new Error('Supplied tarball requires a lowercase SHA-256 digest')
  }
  const actual = crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex')
  if (actual !== digest) throw new Error('Supplied tarball digest mismatch')
  return file
}

module.exports = { suppliedPackedTarball }
