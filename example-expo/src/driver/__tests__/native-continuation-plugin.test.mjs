import assert from 'node:assert/strict'
import { createRequire } from 'node:module'
import test from 'node:test'
import { spawnSync } from 'node:child_process'
import { fileURLToPath } from 'node:url'

const require = createRequire(import.meta.url)
const repositoryRoot = fileURLToPath(new URL('../../../../', import.meta.url))
const { registerPackage, registerIosSources } = require('../../../plugins/with-native-continuation.cjs')

test('app-only native package registration is exact and idempotent', () => {
  const source = 'PackageList(this).packages.apply {\n // existing\n}'
  const result = registerPackage(source)
  assert.match(result, /add\(com\.sfourdrinier\.bleplxexample\.continuation\.ReferenceContinuationPackage\(\)\)/)
  assert.match(result, /\/\/ existing/)
  assert.equal(registerPackage(result), result)
})

test('unsupported application template refuses instead of silently missing the native adapter', () => {
  assert.throws(() => registerPackage('unrecognized application'), /MainApplication/)
})

test('both Apple source files enter the application sources without a bridging-header mutation', () => {
  const project = {}
  const calls = []
  registerIosSources(project, 'Example', { addBuildSourceFileToGroup(options) { calls.push(options) } })
  assert.deepEqual(calls, ['swift', 'm'].map(extension => ({
    filepath: `Example/ReferenceContinuationModule.${extension}`, groupName: 'Example', project
  })))
})

test('real Xcode project registration is idempotent and adds both application source build entries', () => {
  const expoRequire = createRequire(require.resolve('expo/config-plugins'))
  const configRequire = createRequire(expoRequire.resolve('@expo/config-plugins'))
  const xcode = configRequire('xcode')
  const { IOSConfig } = expoRequire('@expo/config-plugins')
  const project = xcode.project('fixture.pbxproj')
  project.hash = { project: { rootObject: 'PROJ', objects: {
    PBXProject: { PROJ: { isa: 'PBXProject', mainGroup: 'ROOT', targets: [{ value: 'TARGET', comment: 'Example' }] } },
    PBXGroup: { ROOT: { isa: 'PBXGroup', children: [{ value: 'APP', comment: 'Example' }], sourceTree: '"<group>"' },
      APP: { isa: 'PBXGroup', path: 'Example', name: 'Example', children: [], sourceTree: '"<group>"' } },
    PBXNativeTarget: { TARGET: { isa: 'PBXNativeTarget', name: 'Example', productType: '"com.apple.product-type.application"', buildPhases: [{ value: 'SOURCES', comment: 'Sources' }] } },
    PBXSourcesBuildPhase: { SOURCES: { isa: 'PBXSourcesBuildPhase', files: [] }, SOURCES_comment: 'Sources' },
    PBXFileReference: {}, PBXBuildFile: {}
  } } }
  registerIosSources(project, 'Example', IOSConfig.XcodeUtils)
  const first = project.writeSync()
  registerIosSources(project, 'Example', IOSConfig.XcodeUtils)
  assert.equal(project.writeSync(), first)
  const files = project.hash.project.objects.PBXSourcesBuildPhase.SOURCES.files
  assert.deepEqual(files.map(file => file.comment).sort(), ['ReferenceContinuationModule.m in Sources', 'ReferenceContinuationModule.swift in Sources'])
  assert.equal(project.hash.project.objects.PBXGroup.APP.children.length, 2)
  assert.doesNotMatch(first, /SWIFT_OBJC_BRIDGING_HEADER/)
})

test('native source templates are trackable while generated Expo projects remain ignored', () => {
  for (const file of ['native/android/ReferenceContinuationModule.kt', 'native/ios/ReferenceContinuationModule.swift']) {
    assert.equal(spawnSync('git', ['check-ignore', '--no-index', '-q', `example-expo/${file}`], { cwd: repositoryRoot }).status, 1, file)
  }
  for (const file of ['android/app/src/main/AndroidManifest.xml', 'ios/generated-placeholder']) {
    assert.equal(spawnSync('git', ['check-ignore', '--no-index', '-q', `example-expo/${file}`], { cwd: repositoryRoot }).status, 0, file)
  }
})
