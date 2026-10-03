// SPDX-License-Identifier: GPL-2.0-or-later
// Shared by source-asset verification and the retained standalone bundle owner.
function deploymentRelease(distribution) {
  if (JSON.stringify(distribution?.linuxAuthorityContract) !== '[1,1,1]')
    throw new Error('deployment authority identity is not ready')
  const release = distribution.release
  const match = typeof release === 'string' ? /^5\.87-ubm\.[1-9][0-9]*$/.exec(release) : null
  if (!match || match[0] !== release) throw new Error('deployment release identity must be a versioned 5.87-ubm.N')
  return release
}

module.exports = { deploymentRelease }
