// example-expo/metro.config.js
//
// The test-driver scenarios live in ../examples-shared/driver so every host
// runs the same code. Metro must watch that folder, and a bare
// `unified-ble-manager` import made from there must resolve to this app's own
// installed copy (one package instance, the one the native module registers
// against), never to the checkout root by package self-reference.

const path = require('node:path')
const { getDefaultConfig } = require('expo/metro-config')

const projectRoot = __dirname
const sharedDriverRoot = path.resolve(projectRoot, '../examples-shared')
const APP_RESOLVED_PACKAGES = /^unified-ble-manager(\/|$)/

const config = getDefaultConfig(projectRoot)
config.watchFolders = [...(config.watchFolders ?? []), sharedDriverRoot]

const upstreamResolveRequest = config.resolver.resolveRequest
config.resolver.resolveRequest = (context, moduleName, platform) => {
  const fromShared = context.originModulePath.startsWith(sharedDriverRoot + path.sep)
  const resolutionContext =
    fromShared && APP_RESOLVED_PACKAGES.test(moduleName)
      ? { ...context, originModulePath: path.join(projectRoot, 'package.json') }
      : context
  return upstreamResolveRequest
    ? upstreamResolveRequest(resolutionContext, moduleName, platform)
    : resolutionContext.resolveRequest(resolutionContext, moduleName, platform)
}

module.exports = config
