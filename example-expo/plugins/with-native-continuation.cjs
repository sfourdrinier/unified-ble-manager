'use strict'

const fs = require('node:fs/promises')
const path = require('node:path')
const registration = 'add(com.sfourdrinier.bleplxexample.continuation.ReferenceContinuationPackage())'
const driverBundleInput = `// UBM reference remote endpoint is an embedded bundle input, not a library option.
tasks.matching {
  it.name.startsWith("createBundle") && it.name.endsWith("JsAndAssets")
}.configureEach {
  inputs.property("ubmReferenceDriverUrl", providers.environmentVariable("EXPO_PUBLIC_UBM_DRIVER_URL").orElse(""))
}`

function registerDriverBundleInputs(source) {
  if (source.includes(driverBundleInput)) return source
  return `${source}\n${driverBundleInput}\n`
}

function registerPackage(source) {
  if (source.includes(registration)) return source
  const anchor = 'PackageList(this).packages.apply {'
  if (!source.includes(anchor))
    throw new Error('Reference continuation requires the Kotlin MainApplication package-list template')
  return source.replace(anchor, `${anchor}\n          ${registration}`)
}

function withNativeContinuation(config) {
  const {
    withMainApplication,
    withAppBuildGradle,
    withDangerousMod,
    withXcodeProject,
    IOSConfig
  } = require('expo/config-plugins')
  config = withAppBuildGradle(config, config => {
    if (config.modResults.language !== 'groovy')
      throw new Error('Reference driver bundle inputs require the Groovy app build template')
    config.modResults.contents = registerDriverBundleInputs(config.modResults.contents)
    return config
  })
  config = withMainApplication(config, config => {
    config.modResults.contents = registerPackage(config.modResults.contents)
    return config
  })
  config = withDangerousMod(config, [
    'android',
    async config => {
      const target = path.join(
        config.modRequest.platformProjectRoot,
        'app/src/main/java/com/sfourdrinier/bleplxexample/continuation'
      )
      await fs.mkdir(target, { recursive: true })
      for (const name of ['ReferenceContinuationModule.kt', 'ReferenceContinuationPackage.kt']) {
        await fs.copyFile(path.join(__dirname, '../native/android', name), path.join(target, name))
      }
      return config
    }
  ])
  return withXcodeProject(config, async config => {
    const projectName = IOSConfig.XcodeUtils.getProjectName(config.modRequest.projectRoot)
    for (const name of ['ReferenceContinuationModule.swift', 'ReferenceContinuationModule.m']) {
      await fs.copyFile(
        path.join(__dirname, '../native/ios', name),
        path.join(config.modRequest.platformProjectRoot, projectName, name)
      )
    }
    registerIosSources(config.modResults, projectName, IOSConfig.XcodeUtils)
    return config
  })
}

function registerIosSources(project, projectName, utils) {
  for (const name of ['ReferenceContinuationModule.swift', 'ReferenceContinuationModule.m']) {
    utils.addBuildSourceFileToGroup({ filepath: `${projectName}/${name}`, groupName: projectName, project })
  }
}

module.exports = withNativeContinuation
module.exports.registerPackage = registerPackage
module.exports.registerIosSources = registerIosSources
module.exports.registerDriverBundleInputs = registerDriverBundleInputs
