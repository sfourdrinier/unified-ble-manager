'use strict'

const assert = require('node:assert/strict')

// A probe does not discover or trust a daemon on the caller's behalf. The host
// supplies the current owner after verifying the deployed LE bearer capability.
// Reuse production admission rather than maintaining a second owner validator.
function radioProbeOptions(env) {
  if (env.UBM_RADIO_PLATFORM !== 'bluez') return {}
  assert.ok(env.UBM_BLUEZ_DAEMON_OWNER !== undefined, 'BlueZ probes require UBM_BLUEZ_DAEMON_OWNER')
  const { admitBluezConnectionPolicy } = require('../../lib/commonjs/backends/desktop/bluez-connection-policy')
  return {
    connectionPolicy: admitBluezConnectionPolicy({
      mode: 'le-bearer',
      daemonUniqueOwner: env.UBM_BLUEZ_DAEMON_OWNER
    })
  }
}

module.exports = { radioProbeOptions }
