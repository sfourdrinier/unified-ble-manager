'use strict'

// Shared trusted launch configuration for Node and Electron main. Never accepts
// renderer/scenario input; the public factory owns daemon-name validation.
function trustedDesktopOptions(backend, adapterId, daemonUniqueOwner) {
  if (adapterId !== undefined && adapterId.trim().length === 0)
    throw new Error('--adapter needs a nonempty exact adapter ID')
  if (daemonUniqueOwner !== undefined && backend !== 'bluez')
    throw new Error('--bluez-daemon-owner is only valid with the bluez backend')
  return {
    ...(adapterId === undefined ? {} : { adapterId }),
    ...(daemonUniqueOwner === undefined ? {} : { connectionPolicy: { mode: 'le-bearer', daemonUniqueOwner } })
  }
}

module.exports = { trustedDesktopOptions }
