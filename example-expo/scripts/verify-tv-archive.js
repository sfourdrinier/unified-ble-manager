'use strict'

const fs = require('node:fs')
const { createHash } = require('node:crypto')

function verifyTvArchive(archive, digestFile) {
  const digest = fs.readFileSync(digestFile, 'utf8').trim()
  if (!/^[a-f0-9]{40}$/u.test(digest)) throw new Error('React Native TV archive has invalid SHA-1 sidecar')
  const actual = createHash('sha1').update(fs.readFileSync(archive)).digest('hex')
  if (actual !== digest) throw new Error('React Native TV archive digest mismatch; remove the corrupt cache entry')
}

module.exports = { verifyTvArchive }
if (require.main === module) verifyTvArchive(...process.argv.slice(2))
