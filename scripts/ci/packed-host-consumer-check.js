#!/usr/bin/env node

'use strict'

const assert = require('assert')
const fs = require('fs')
const os = require('os')
const path = require('path')
const { spawnSync } = require('child_process')

const root = path.resolve(__dirname, '../..')
const rootPackage = require(path.join(root, 'package.json'))
const PACK_INSTALL_CHILD_TIMEOUT_MS = 600000
const TYPESCRIPT_VERSION = '5.8.3'

/** Public runtime/type names that each packaged host entrypoint must expose. */
const PACKED_HOST_CONSUMER_CONTRACTS = Object.freeze({
  expo: Object.freeze({
    runtimeExports: Object.freeze(['createExpoBleManager', 'createExpoBleManagerWithEnvironment'])
  }),
  tauri: Object.freeze({
    runtimeExports: Object.freeze([
      'createTauriBleManager',
      'createTauriBleManagerWithEnvironment',
      'createTauriBleProvider'
    ])
  }),
  react: Object.freeze({
    runtimeExports: Object.freeze([
      'BleProvider',
      'getAdapterState',
      'getBleCapability',
      'getBleReadiness',
      'useAdapterState',
      'useBle',
      'useBleCapability',
      'useBleReadiness',
      'useCharacteristicValue',
      'useConnectionState',
      'useDiscoveredPeers'
    ])
  })
})

function npmCommand() {
  return process.platform === 'win32' ? 'npm.cmd' : 'npm'
}

function collectRuntimeTargets(value, targets = []) {
  if (value === undefined) return targets
  if (typeof value === 'string') {
    targets.push(value)
    return targets
  }
  if (value === null || typeof value !== 'object' || Array.isArray(value)) {
    throw new Error('package export must be a string or conditional export object')
  }
  for (const [condition, target] of Object.entries(value)) {
    if (condition !== 'types') collectRuntimeTargets(target, targets)
  }
  return targets
}

/**
 * Finds host entrypoints by their emitted public target, not by a duplicated
 * package-subpath list. A renamed export key remains covered as long as it
 * still points at the canonical host artifact.
 */
function derivePackedHostConsumerExports(exportsMap) {
  const result = []
  for (const [exportPath, exportTarget] of Object.entries(exportsMap)) {
    const matchingHosts = Object.keys(PACKED_HOST_CONSUMER_CONTRACTS).filter(
      host =>
        collectRuntimeTargets(exportTarget.import).some(target => target.endsWith(`/lib/module/${host}.js`)) &&
        collectRuntimeTargets(exportTarget.require).some(target => target.endsWith(`/lib/commonjs/${host}.js`))
    )
    if (matchingHosts.length > 1) {
      throw new Error(`package export maps to multiple packed host contracts: ${exportPath}`)
    }
    if (matchingHosts.length === 1) {
      result.push({ exportPath, host: matchingHosts[0] })
    }
  }
  return result.sort((left, right) => left.exportPath.localeCompare(right.exportPath))
}

function assertChildProcessResult(command, result, output, cwd) {
  if (result.error?.code === 'ETIMEDOUT') {
    throw new Error(`${command} timed out after ${String(PACK_INSTALL_CHILD_TIMEOUT_MS)}ms (cwd: ${cwd})\n${output}`)
  }
  if (result.error) throw new Error(`${command} could not start: ${result.error.message}`)
  if (result.signal !== null)
    throw new Error(`${command} terminated by signal ${result.signal} (cwd: ${cwd})\n${output}`)
  if (result.status !== 0) throw new Error(`${command} failed (${String(result.status)}):\n${output}`)
  if (
    /^(?:npm )?(?:WARN|warn)\b|^warning\b|^⚠|(?:^|\n).*?(?:DeprecationWarning|\bdeprecated\b|\bdeprecation\b)/im.test(
      output
    )
  ) {
    throw new Error(`${command} produced a warning:\n${output}`)
  }
}

function run(command, args, options = {}) {
  const cwd = options.cwd || root
  const result = spawnSync(command, args, {
    cwd,
    encoding: 'utf8',
    env: { ...process.env, ...(options.env || {}) },
    shell: false,
    timeout: options.timeoutMs || PACK_INSTALL_CHILD_TIMEOUT_MS
  })
  const output = `${result.stdout || ''}${result.stderr || ''}`
  assertChildProcessResult(`${command} ${args.join(' ')}`, result, output, cwd)
  return result.stdout || ''
}

function tarballName(packageName, version) {
  return `${packageName.replace(/^@/, '').replace('/', '-')}-${version}.tgz`
}

function writePeerStubs(tmp) {
  const reactStub = path.join(tmp, 'react-stub')
  const reactNativeStub = path.join(tmp, 'react-native-stub')
  fs.mkdirSync(reactStub)
  fs.mkdirSync(reactNativeStub)
  fs.writeFileSync(
    path.join(reactStub, 'package.json'),
    JSON.stringify({ name: 'react', version: '19.0.0', main: 'index.js', types: 'index.d.ts' })
  )
  fs.writeFileSync(
    path.join(reactStub, 'index.js'),
    [
      'function createContext(value) { return { Provider: {}, value }; }',
      'function createElement(type, props, ...children) { return { type, props, children }; }',
      'function useContext(context) { return context.value; }',
      'function useEffect() {}',
      'function useMemo(factory) { return factory(); }',
      "function useState(initial) { return [typeof initial === 'function' ? initial() : initial, () => {}]; }",
      'module.exports = { createContext, createElement, useContext, useEffect, useMemo, useState };',
      ''
    ].join('\n')
  )
  fs.writeFileSync(
    path.join(reactStub, 'index.d.ts'),
    [
      'export type ReactNode = unknown',
      'export interface ReactElement { readonly type?: unknown; readonly props?: unknown }',
      'export interface Context<T> { readonly Provider: unknown; readonly value: T }',
      'export function createContext<T>(value: T): Context<T>',
      'export function createElement(type: unknown, props: unknown, ...children: readonly unknown[]): ReactElement',
      'export function useContext<T>(context: Context<T>): T',
      'export function useEffect(effect: () => void | (() => void), dependencies?: readonly unknown[]): void',
      'export function useMemo<T>(factory: () => T, dependencies: readonly unknown[]): T',
      'export function useState<T>(initial: T | (() => T)): [T, (value: T) => void]',
      ''
    ].join('\n')
  )
  fs.writeFileSync(
    path.join(reactNativeStub, 'package.json'),
    JSON.stringify({ name: 'react-native', version: '0.86.0', main: 'index.js', types: 'index.d.ts' })
  )
  fs.writeFileSync(
    path.join(reactNativeStub, 'index.js'),
    [
      'class NativeEventEmitter { addListener() { return { remove() {} }; } }',
      "const Platform = { OS: 'test', Version: 0, select: values => values.default };",
      'const PermissionsAndroid = { PERMISSIONS: {}, RESULTS: {}, check: async () => false, requestMultiple: async () => ({}) };',
      'const TurboModuleRegistry = { getEnforcing: () => ({ getConstants: () => ({}) }) };',
      'module.exports = { NativeEventEmitter, Platform, PermissionsAndroid, TurboModuleRegistry };',
      ''
    ].join('\n')
  )
  fs.writeFileSync(
    path.join(reactNativeStub, 'index.d.ts'),
    'declare const ReactNative: Record<string, unknown>\nexport = ReactNative\n'
  )
}

function assertInstalledFromTarball(consumer) {
  const packageRoot = fs.realpathSync(path.join(consumer, 'node_modules', rootPackage.name))
  assert.ok(
    fs.existsSync(path.join(packageRoot, 'package.json')),
    `installed package manifest is missing: ${packageRoot}`
  )
  const consumerRoot = fs.realpathSync(consumer)
  assert.ok(
    packageRoot.startsWith(path.join(consumerRoot, 'node_modules', rootPackage.name)),
    `consumer resolved outside installed package: ${packageRoot}`
  )
  assert.ok(!packageRoot.startsWith(root), `consumer resolved repository source instead of tarball: ${packageRoot}`)
  return packageRoot
}

function writeTypeScriptFixture(consumer, module, moduleResolution) {
  const fixtureDirectory = path.join(consumer, `typescript-${moduleResolution.toLowerCase()}`)
  fs.mkdirSync(fixtureDirectory)
  fs.writeFileSync(
    path.join(fixtureDirectory, 'host-consumer.ts'),
    [
      "import { createExpoBleManager, createExpoBleManagerWithEnvironment } from 'unified-ble-manager/expo'",
      "import type { ExpoBleManagerEnvironment } from 'unified-ble-manager/expo'",
      "import { createTauriBleManager, createTauriBleManagerWithEnvironment, createTauriBleProvider } from 'unified-ble-manager/tauri'",
      "import type { TauriBleManagerEnvironment, TauriBleProvider } from 'unified-ble-manager/tauri'",
      "import { BleProvider, getAdapterState, getBleCapability, getBleReadiness, useAdapterState, useBle, useBleCapability, useBleReadiness, useCharacteristicValue, useConnectionState, useDiscoveredPeers } from 'unified-ble-manager/react'",
      "import type { BleProviderProps } from 'unified-ble-manager/react'",
      '',
      'declare const expoEnvironment: ExpoBleManagerEnvironment',
      'declare const tauriEnvironment: TauriBleManagerEnvironment',
      'declare const tauriProvider: TauriBleProvider',
      'declare const reactEnvironment: BleProviderProps',
      '',
      'void createExpoBleManager',
      'void createExpoBleManagerWithEnvironment(expoEnvironment)',
      'void createTauriBleManager',
      'void createTauriBleManagerWithEnvironment(tauriEnvironment)',
      'void createTauriBleProvider',
      'void tauriProvider',
      'void BleProvider',
      'void getAdapterState',
      'void getBleCapability',
      'void getBleReadiness',
      'void useAdapterState',
      'void useBle',
      'void useBleCapability',
      'void useBleReadiness',
      'void useCharacteristicValue',
      'void useConnectionState',
      'void useDiscoveredPeers',
      'void reactEnvironment',
      ''
    ].join('\n')
  )
  fs.writeFileSync(
    path.join(fixtureDirectory, 'tsconfig.json'),
    `${JSON.stringify(
      {
        compilerOptions: {
          lib: ['ES2022', 'DOM'],
          strict: true,
          noEmit: true,
          noUnusedLocals: true,
          noUnusedParameters: true,
          target: 'ES2022',
          module,
          moduleResolution,
          skipLibCheck: true
        },
        include: ['host-consumer.ts']
      },
      null,
      2
    )}\n`
  )
  return fixtureDirectory
}

function runTypeScriptConsumers(consumer) {
  const tsc = path.join(consumer, 'node_modules', 'typescript', 'bin', 'tsc')
  assert.ok(fs.existsSync(tsc), `isolated TypeScript compiler is missing: ${tsc}`)
  for (const configuration of [
    { module: 'ESNext', moduleResolution: 'Bundler' },
    { module: 'NodeNext', moduleResolution: 'NodeNext' }
  ]) {
    const fixtureDirectory = writeTypeScriptFixture(consumer, configuration.module, configuration.moduleResolution)
    run(process.execPath, [tsc, '--project', path.join(fixtureDirectory, 'tsconfig.json')], { cwd: consumer })
  }
}

function hostSpecifier(exportPath) {
  return `${rootPackage.name}${exportPath.slice(1)}`
}

function runCjsConsumer(consumer, entries) {
  const assertions = [
    "const assert = require('assert');",
    "const path = require('path');",
    `const packageRoot = ${JSON.stringify(assertInstalledFromTarball(consumer))};`
  ]
  for (const entry of entries) {
    const contract = PACKED_HOST_CONSUMER_CONTRACTS[entry.host]
    const specifier = hostSpecifier(entry.exportPath)
    assertions.push(`const ${entry.host} = require(${JSON.stringify(specifier)});`)
    for (const exportName of contract.runtimeExports) {
      assertions.push(
        `assert.strictEqual(typeof ${entry.host}[${JSON.stringify(exportName)}], 'function', ${JSON.stringify(`${specifier}.${exportName} is a function`)});`
      )
    }
  }
  assertions.push(
    "assert.ok(packageRoot.includes(path.join('node_modules', 'unified-ble-manager')), 'CJS consumer uses installed package');"
  )
  assertions.push("console.log('packed-host-consumer CJS Expo/Tauri/React imports ok');")
  run(process.execPath, ['-e', assertions.join('\n')], { cwd: consumer })
}

function runEsmConsumer(consumer, entries) {
  const assertions = [
    "import assert from 'node:assert/strict';",
    "import path from 'node:path';",
    `const packageRoot = ${JSON.stringify(assertInstalledFromTarball(consumer))};`
  ]
  for (const entry of entries) {
    const contract = PACKED_HOST_CONSUMER_CONTRACTS[entry.host]
    const specifier = hostSpecifier(entry.exportPath)
    assertions.push(`const ${entry.host} = await import(${JSON.stringify(specifier)});`)
    for (const exportName of contract.runtimeExports) {
      assertions.push(
        `assert.equal(typeof ${entry.host}[${JSON.stringify(exportName)}], 'function', ${JSON.stringify(`${specifier}.${exportName} is a function`)});`
      )
    }
  }
  assertions.push(
    "assert.ok(packageRoot.includes(path.join('node_modules', 'unified-ble-manager')), 'ESM consumer uses installed package');"
  )
  assertions.push("console.log('packed-host-consumer ESM Expo/Tauri/React imports ok');")
  run(process.execPath, ['--input-type=module', '-e', assertions.join('\n')], { cwd: consumer })
}

function removeTemporaryDirectory(directory) {
  const resolved = path.resolve(directory)
  const temporaryRoot = path.resolve(os.tmpdir())
  const relative = path.relative(temporaryRoot, resolved)
  if (
    relative === '' ||
    relative === '..' ||
    relative.startsWith(`..${path.sep}`) ||
    !path.basename(resolved).startsWith('ubm-packed-host-consumer-')
  ) {
    throw new Error(`refusing to remove unexpected packed host consumer directory: ${resolved}`)
  }
  fs.rmSync(resolved, { recursive: true, force: true })
}

function main() {
  const entries = derivePackedHostConsumerExports(rootPackage.exports)
  const expectedHosts = Object.keys(PACKED_HOST_CONSUMER_CONTRACTS)
  for (const host of expectedHosts) {
    assert.ok(
      entries.some(entry => entry.host === host),
      `package exports do not expose packed ${host} host`
    )
  }

  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-packed-host-consumer-'))
  try {
    const artifactDirectory = path.join(tmp, 'artifacts')
    fs.mkdirSync(artifactDirectory)
    writePeerStubs(tmp)
    const npmCache = path.join(tmp, 'npm-cache')
    const npmEnvironment = {
      NPM_CONFIG_AUDIT: 'false',
      NPM_CONFIG_CACHE: npmCache,
      NPM_CONFIG_UPDATE_NOTIFIER: 'false'
    }
    const tarballPath = path.join(artifactDirectory, tarballName(rootPackage.name, rootPackage.version))
    run(npmCommand(), ['pack', '--ignore-scripts', '--pack-destination', artifactDirectory, '--loglevel=warn'], {
      cwd: root,
      env: npmEnvironment
    })
    assert.ok(fs.existsSync(tarballPath), `packed tarball is missing: ${tarballPath}`)
    run(process.execPath, ['scripts/ci/verify-package-tarballs.js', tarballPath], { cwd: root })

    const consumer = path.join(tmp, 'consumer')
    fs.mkdirSync(consumer)
    fs.writeFileSync(
      path.join(consumer, 'package.json'),
      `${JSON.stringify(
        {
          name: 'ubm-packed-host-consumer',
          private: true,
          version: '0.0.0',
          dependencies: {
            '@tauri-apps/api': '2.0.0',
            expo: '57.0.9',
            react: 'file:../react-stub',
            'react-native': 'file:../react-native-stub',
            [rootPackage.name]: tarballPath
          },
          devDependencies: { typescript: TYPESCRIPT_VERSION }
        },
        null,
        2
      )}\n`
    )
    run(npmCommand(), ['install', '--ignore-scripts', '--include=dev', '--prefer-offline', '--loglevel=error'], {
      cwd: consumer,
      env: npmEnvironment
    })

    runCjsConsumer(consumer, entries)
    runEsmConsumer(consumer, entries)
    runTypeScriptConsumers(consumer)

    const result = {
      schema: 'unified-ble-packed-host-consumer-proof-v1',
      status: 'deterministic-packed-artifact-proof',
      artifact: {
        packageName: rootPackage.name,
        packageVersion: rootPackage.version,
        installedFrom: 'packed-tarball',
        sourcePathUsedByConsumers: false
      },
      hosts: entries.map(entry => entry.host),
      physicalRadio: 'not-provided'
    }
    console.log(JSON.stringify(result))
    return result
  } finally {
    removeTemporaryDirectory(tmp)
  }
}

if (require.main === module) {
  try {
    main()
  } catch (error) {
    console.error(error)
    process.exitCode = 1
  }
}

module.exports = {
  PACKED_HOST_CONSUMER_CONTRACTS,
  derivePackedHostConsumerExports,
  main
}
