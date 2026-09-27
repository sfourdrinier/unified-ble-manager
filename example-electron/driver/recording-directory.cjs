'use strict'

const { mkdir } = require('node:fs/promises')
const { isAbsolute } = require('node:path')

/** Trusted application path only. New POSIX directories are private; existing
 * permissions remain the application's responsibility. No radio is acquired. */
function createRecordingDirectory(directory, makeDirectory = mkdir) {
  if (typeof directory !== 'string' || !isAbsolute(directory)) throw new Error('recording directory must be absolute')
  let pending = null
  return () => {
    if (pending === null) {
      pending = Promise.resolve()
        .then(() => makeDirectory(directory, { recursive: true, mode: 0o700 }))
        .catch(error => {
          pending = null
          throw error
        })
    }
    return pending
  }
}

module.exports = { createRecordingDirectory }
