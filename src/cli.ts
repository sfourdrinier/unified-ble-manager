// src/cli.ts

import { mkdir, readFile, stat, writeFile } from 'node:fs/promises'
import { createRequire } from 'node:module'
import { homedir } from 'node:os'
import { dirname, isAbsolute, resolve } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import type { BleCentralBackend } from './backend-contract/backend'
import type { BackendIdentity, HostKind } from './backend-contract/identity'
import type { SerializableRecord, SerializableValue } from './backend-contract/primitives'
import type { CapabilityLimits } from './backend-contract/capabilities'
import {
  createBackendAuthorDefinition,
  inspectBackendCapabilities,
  runBackendAuthorTck,
  type BackendAuthoringDefinition,
  type BackendCapabilityReport
} from './backend-sdk-authoring'
import { parseCliJson } from './cli-json'
import {
  redactTraceDocument,
  validateTraceDocument,
  type DiagnosticTraceDocument,
  type TraceValidationResult
} from './diagnostics/trace-format'
import { baseTckScenarios } from './tck/scenarios'
import { UNIFIED_BLE_IMPLEMENTATION_VERSION } from './implementation-version'
import { TAURI_PLUGIN_COMPATIBILITY } from './tauri/compatibility'

const maximumTraceFileBytes = 1024 * 1024
const requireFromCliWorkingDirectory = createRequire(
  pathToFileURL(resolve(process.cwd(), 'unified-ble-manager-cli-loader.cjs')).href
)

export type UnifiedBleCliCommand =
  | 'doctor'
  | 'capabilities'
  | 'trace'
  | 'tck'
  | 'scenario'
  | 'init'
  | 'inspect'
  | 'support-bundle'

type CliHost = 'expo' | 'tauri' | 'electron' | 'node' | 'web'

export interface UnifiedBleCliFailure {
  readonly code:
    | 'cli.argument-invalid'
    | 'cli.backend-load-failed'
    | 'cli.backend-invalid'
    | 'cli.host-unsupported'
    | 'cli.trace-invalid'
    | 'cli.execution-failed'
  readonly message: string
}

export interface UnifiedBleCliResult {
  readonly ok: boolean
  readonly command: UnifiedBleCliCommand | null
  readonly data: SerializableRecord | null
  readonly failures: readonly UnifiedBleCliFailure[]
}

export type CliBackendDefinition = BackendAuthoringDefinition<
  string,
  BackendIdentity<string>,
  BleCentralBackend<string, BackendIdentity<string>>
>

export interface UnifiedBleCliRuntime {
  readTextFile(path: string): Promise<string>
  loadBackendModule(moduleSpecifier: string): Promise<CliBackendDefinition>
  writeTextFile?(path: string, contents: string): Promise<void>
  cwd?(): string
}

/**
 * Runs non-interactive, Node-only diagnostics. Consumer commands never select
 * a radio. Backend-authoring commands require an explicit --backend module.
 */
export async function runUnifiedBleCli(
  argumentsValue: readonly string[],
  runtime: UnifiedBleCliRuntime = defaultCliRuntime
): Promise<UnifiedBleCliResult> {
  const invocation = parseInvocation(argumentsValue)
  if (invocation instanceof UnifiedBleCliFailureError) {
    return failure(invocation.command, invocation.code, invocation.message)
  }
  try {
    if (invocation.kind === 'consumer-doctor') {
      return runConsumerDoctorCommand(runtime)
    }
    if (invocation.kind === 'inspect-package') {
      return runInspectPackageCommand(runtime)
    }
    if (invocation.kind === 'inspect-config') {
      return runInspectConfigCommand(invocation.host)
    }
    if (invocation.kind === 'inspect-capabilities') {
      return runInspectCapabilitiesCommand(invocation.host)
    }
    if (invocation.kind === 'init') {
      return runInitCommand(invocation, runtime)
    }
    if (invocation.kind === 'support-bundle') {
      return runSupportBundleCommand(invocation, runtime)
    }
    if (invocation.command === 'trace') {
      return runTraceCommand(invocation, runtime)
    }
    const definition = await loadNodeBackendDefinition(invocation.backendModule, runtime)
    if (invocation.command === 'doctor') {
      return runDoctorCommand(definition)
    }
    if (invocation.command === 'capabilities') {
      return runCapabilitiesCommand(definition)
    }
    if (invocation.command === 'tck') {
      return runTckCommand(definition)
    }
    return runScenarioCommand(definition, invocation.scenarioId)
  } catch (error) {
    if (error instanceof UnifiedBleCliFailureError) {
      return failure(invocation.command, error.code, error.message)
    }
    const message = error instanceof Error ? error.message : 'operation threw a non-Error value'
    return failure(invocation.command, 'cli.execution-failed', message)
  }
}

interface ParsedTraceInvocation {
  readonly kind?: 'trace'
  readonly command: 'trace'
  readonly action: 'validate' | 'redact'
  readonly tracePath: string
}

interface ParsedBackendInvocation {
  readonly kind?: 'backend'
  readonly command: Exclude<UnifiedBleCliCommand, 'trace' | 'init' | 'inspect' | 'support-bundle'>
  readonly backendModule: string
  readonly scenarioId: string | null
}

interface ParsedConsumerDoctorInvocation {
  readonly kind: 'consumer-doctor'
  readonly command: 'doctor'
}

interface ParsedInspectPackageInvocation {
  readonly kind: 'inspect-package'
  readonly command: 'inspect'
}

interface ParsedInspectConfigInvocation {
  readonly kind: 'inspect-config'
  readonly command: 'inspect'
  readonly host: CliHost
}

interface ParsedInspectCapabilitiesInvocation {
  readonly kind: 'inspect-capabilities'
  readonly command: 'inspect'
  readonly host: CliHost
}

interface ParsedInitInvocation {
  readonly kind: 'init'
  readonly command: 'init'
  readonly host: CliHost
  readonly directory: string | null
  readonly force: boolean
}

interface ParsedSupportBundleInvocation {
  readonly kind: 'support-bundle'
  readonly command: 'support-bundle'
  readonly output: string
  readonly force: boolean
}

type ParsedInvocation =
  | ParsedTraceInvocation
  | ParsedBackendInvocation
  | ParsedConsumerDoctorInvocation
  | ParsedInspectPackageInvocation
  | ParsedInspectConfigInvocation
  | ParsedInspectCapabilitiesInvocation
  | ParsedInitInvocation
  | ParsedSupportBundleInvocation

function parseInvocation(argumentsValue: readonly string[]): ParsedInvocation | UnifiedBleCliFailureError {
  const command = argumentsValue[0]
  if (command === 'backend') {
    return parseInvocation(argumentsValue.slice(1))
  }
  if (command === 'inspect') {
    return parseInspectInvocation(argumentsValue.slice(1))
  }
  if (command === 'init') {
    return parseInitInvocation(argumentsValue.slice(1))
  }
  if (command === 'support-bundle') {
    return parseSupportBundleInvocation(argumentsValue.slice(1))
  }
  if (command === 'doctor') {
    const rest = argumentsValue.slice(1)
    if (rest.length === 0 || (rest.length === 1 && rest[0] === '--json')) {
      return { kind: 'consumer-doctor', command: 'doctor' }
    }
  }
  if (!isCliCommand(command) || command === 'init' || command === 'inspect' || command === 'support-bundle') {
    return new UnifiedBleCliFailureError(
      'cli.argument-invalid',
      'expected doctor, inspect, init, support-bundle, capabilities, trace, tck, or scenario'
    )
  }
  if (command === 'trace') {
    const action = argumentsValue[1]
    const tracePath = argumentsValue[2]
    if ((action !== 'validate' && action !== 'redact') || tracePath === undefined || argumentsValue.length !== 3) {
      return new UnifiedBleCliFailureError('cli.argument-invalid', 'trace requires validate|redact <file>')
    }
    return { command, action, tracePath }
  }
  const options = parseOptions(argumentsValue.slice(1), command)
  if (options instanceof UnifiedBleCliFailureError) {
    return options
  }
  const backendModule = options.get('backend')
  if (backendModule === undefined) {
    return new UnifiedBleCliFailureError('cli.argument-invalid', `${command} requires --backend <module>`, command)
  }
  const scenarioId = options.get('scenario')
  if (command === 'scenario' && scenarioId === undefined) {
    return new UnifiedBleCliFailureError('cli.argument-invalid', 'scenario requires --scenario <id>', command)
  }
  if (command !== 'scenario' && scenarioId !== undefined) {
    return new UnifiedBleCliFailureError('cli.argument-invalid', `${command} does not accept --scenario`, command)
  }
  return { command, backendModule, scenarioId: scenarioId ?? null }
}

function parseOptions(
  argumentsValue: readonly string[],
  command: Exclude<UnifiedBleCliCommand, 'trace'>
): ReadonlyMap<string, string> | UnifiedBleCliFailureError {
  const allowed = command === 'scenario' ? new Set(['backend', 'scenario']) : new Set(['backend'])
  const options = new Map<string, string>()
  for (let index = 0; index < argumentsValue.length; index += 1) {
    const token = argumentsValue[index]
    if (token === undefined || !token.startsWith('--')) {
      return new UnifiedBleCliFailureError('cli.argument-invalid', `${command} accepts named options only`)
    }
    const name = token.slice(2)
    if (!allowed.has(name)) {
      return new UnifiedBleCliFailureError('cli.argument-invalid', `${command} does not accept --${name}`)
    }
    if (options.has(name)) {
      return new UnifiedBleCliFailureError('cli.argument-invalid', `--${name} may be provided once`)
    }
    const value = argumentsValue[index + 1]
    if (value === undefined || value.startsWith('--') || value.length === 0) {
      return new UnifiedBleCliFailureError('cli.argument-invalid', `--${name} requires a value`)
    }
    options.set(name, value)
    index += 1
  }
  return options
}

async function loadNodeBackendDefinition(
  moduleSpecifier: string,
  runtime: UnifiedBleCliRuntime
): Promise<CliBackendDefinition> {
  let definition: CliBackendDefinition
  try {
    definition = await runtime.loadBackendModule(moduleSpecifier)
  } catch (error) {
    const message = error instanceof Error ? error.message : 'backend module loader threw a non-Error value'
    throw new UnifiedBleCliFailureError('cli.backend-load-failed', `unable to load ${moduleSpecifier}: ${message}`)
  }
  try {
    const verified = createBackendAuthorDefinition(definition)
    assertNodeCapableHost(verified.factory.provider.descriptor.hostKind)
    return verified
  } catch (error) {
    if (error instanceof UnifiedBleCliFailureError) {
      throw error
    }
    const message = error instanceof Error ? error.message : 'backend definition validation threw a non-Error value'
    throw new UnifiedBleCliFailureError(
      'cli.backend-invalid',
      `invalid backend definition from ${moduleSpecifier}: ${message}`
    )
  }
}

function assertNodeCapableHost(hostKind: HostKind): void {
  if (hostKind !== 'node' && hostKind !== 'desktop-native' && hostKind !== 'test') {
    throw new UnifiedBleCliFailureError(
      'cli.host-unsupported',
      `CLI cannot drive ${hostKind}; select an explicit Node-capable backend`
    )
  }
}

function runtimeCwd(runtime: UnifiedBleCliRuntime): string {
  return runtime.cwd === undefined ? process.cwd() : runtime.cwd()
}

async function locateInstalledPackage(cwd: string): Promise<{
  readonly name: string
  readonly version: string
  readonly path: string
} | null> {
  const localManifest = resolve(cwd, 'package.json')
  try {
    const document: unknown = JSON.parse(await readFile(localManifest, 'utf8'))
    const identity = packageIdentity(document)
    if (identity !== null && identity.name === 'unified-ble-manager') {
      return { ...identity, path: localManifest }
    }
  } catch {
    // Fall through to Node package resolution from the consumer cwd.
  }
  try {
    const resolved = createRequire(pathToFileURL(resolve(cwd, 'package.json')).href).resolve(
      'unified-ble-manager/package.json'
    )
    const document: unknown = JSON.parse(await readFile(resolved, 'utf8'))
    const identity = packageIdentity(document)
    if (identity !== null) {
      return { ...identity, path: resolved }
    }
  } catch {
    return null
  }
  return null
}

function packageIdentity(value: unknown): { readonly name: string; readonly version: string } | null {
  if (typeof value !== 'object' || value === null) return null
  const name = Reflect.get(value, 'name')
  const version = Reflect.get(value, 'version')
  if (typeof name !== 'string' || typeof version !== 'string') return null
  return { name, version }
}

async function runConsumerDoctorCommand(runtime: UnifiedBleCliRuntime): Promise<UnifiedBleCliResult> {
  const cwd = runtimeCwd(runtime)
  const installed = await locateInstalledPackage(cwd)
  if (installed === null) {
    return failure(
      'doctor',
      'cli.execution-failed',
      'unified-ble-manager is not installed in this directory; pnpm add unified-ble-manager'
    )
  }
  return success('doctor', {
    schemaVersion: 1,
    proofBoundary: 'compile-config-loadability',
    liveRadio: false,
    package: {
      name: installed.name,
      version: installed.version,
      path: installed.path
    },
    runtime: {
      node: process.version,
      platform: process.platform,
      architecture: process.arch,
      napi: process.versions.napi ?? null
    },
    tauri: {
      npmRange: TAURI_PLUGIN_COMPATIBILITY.npmRange,
      crateRange: TAURI_PLUGIN_COMPATIBILITY.crateRange,
      ipcProtocol: TAURI_PLUGIN_COMPATIBILITY.ipcProtocol
    },
    remediation: Object.freeze([
      'Use an explicit host factory; the root package does not select a radio.',
      'Do not treat this report as physical-radio evidence.'
    ])
  })
}

async function runInspectPackageCommand(runtime: UnifiedBleCliRuntime): Promise<UnifiedBleCliResult> {
  const installed = await locateInstalledPackage(runtimeCwd(runtime))
  if (installed === null) {
    return failure('inspect', 'cli.execution-failed', 'unified-ble-manager is not installed in this directory')
  }
  return consumerSuccess('inspect', {
    name: installed.name,
    version: installed.version,
    path: installed.path,
    proofBoundary: 'compile-config-loadability'
  })
}

function runInspectConfigCommand(host: CliHost): UnifiedBleCliResult {
  const compatibility = {
    npmRange: TAURI_PLUGIN_COMPATIBILITY.npmRange,
    crateRange: TAURI_PLUGIN_COMPATIBILITY.crateRange,
    ipcProtocol: TAURI_PLUGIN_COMPATIBILITY.ipcProtocol
  }
  if (host === 'tauri') {
    return consumerSuccess('inspect', {
      host,
      documentedCrate: 'tauri-plugin-unified-ble-manager',
      documentedInstall: 'crates.io',
      pathDependency: 'checkout-fallback',
      liveRadio: false,
      proofBoundary: 'compile-config-loadability',
      cratePublished: false,
      compatibility
    })
  }
  return consumerSuccess('inspect', {
    host,
    documentedInstall: 'npm',
    liveRadio: false,
    proofBoundary: 'compile-config-loadability',
    factory: hostFactoryName(host)
  })
}

function runInspectCapabilitiesCommand(host: CliHost): UnifiedBleCliResult {
  const data: Record<string, SerializableValue> = {
    host,
    liveRadio: false,
    proofBoundary: 'compile-config-loadability'
  }
  if (host === 'tauri') {
    data.ipcProtocol = TAURI_PLUGIN_COMPATIBILITY.ipcProtocol
  }
  return consumerSuccess('inspect', data)
}

async function runSupportBundleCommand(
  invocation: ParsedSupportBundleInvocation,
  runtime: UnifiedBleCliRuntime
): Promise<UnifiedBleCliResult> {
  const output = isAbsolute(invocation.output) ? invocation.output : resolve(runtimeCwd(runtime), invocation.output)
  try {
    await stat(output)
    if (!invocation.force) {
      return failure('support-bundle', 'cli.argument-invalid', `${output} already exists; pass --force to overwrite`)
    }
  } catch {
    // Missing target is the write path.
  }
  const installed = await locateInstalledPackage(runtimeCwd(runtime))
  if (installed === null) {
    return failure(
      'support-bundle',
      'cli.execution-failed',
      'unified-ble-manager is not installed in this directory; pnpm add unified-ble-manager'
    )
  }
  const document = {
    schemaVersion: 1,
    liveRadio: false,
    proofBoundary: 'compile-config-loadability',
    package: {
      name: installed.name,
      version: installed.version
    },
    runtime: {
      node: process.version,
      platform: process.platform,
      architecture: process.arch
    },
    redaction: {
      peerIds: true,
      payloads: true,
      homePaths: true
    }
  }
  const serialized = redactHomePaths(JSON.stringify(document, null, 2))
  await mkdir(dirname(output), { recursive: true })
  if (runtime.writeTextFile === undefined) {
    await writeFile(output, `${serialized}\n`, 'utf8')
  } else {
    await runtime.writeTextFile(output, `${serialized}\n`)
  }
  return consumerSuccess('support-bundle', {
    path: redactHomePaths(output),
    liveRadio: false,
    proofBoundary: 'compile-config-loadability'
  })
}

function hostFactoryName(host: Exclude<CliHost, 'tauri'>): string {
  if (host === 'expo') return 'createExpoBleManager'
  if (host === 'electron') return 'createElectronRendererBleManager'
  if (host === 'web') return 'createWebBleManager'
  return 'createCoreBluetoothBleManager'
}

function redactHomePaths(value: string): string {
  const home = homedir()
  return home.length === 0 ? value : value.split(home).join('<home>')
}

async function runInitCommand(
  invocation: ParsedInitInvocation,
  runtime: UnifiedBleCliRuntime
): Promise<UnifiedBleCliResult> {
  const directory =
    invocation.directory === null
      ? runtimeCwd(runtime)
      : isAbsolute(invocation.directory)
        ? invocation.directory
        : resolve(runtimeCwd(runtime), invocation.directory)
  const fragments = initFragments(invocation.host)
  for (const fragment of fragments) {
    const target = resolve(directory, fragment.fileName)
    try {
      await stat(target)
      if (!invocation.force) {
        return failure('init', 'cli.argument-invalid', `${target} already exists; pass --force to overwrite`)
      }
    } catch {
      // Missing target is the write path.
    }
  }
  await mkdir(directory, { recursive: true })
  const written: string[] = []
  for (const fragment of fragments) {
    const target = resolve(directory, fragment.fileName)
    if (runtime.writeTextFile === undefined) {
      await writeFile(target, fragment.contents, 'utf8')
    } else {
      await runtime.writeTextFile(target, fragment.contents)
    }
    written.push(target)
  }
  const primary = written[0]
  if (primary === undefined) {
    return failure('init', 'cli.execution-failed', 'init produced no files')
  }
  return consumerSuccess('init', {
    host: invocation.host,
    path: primary,
    files: written,
    packageVersion: UNIFIED_BLE_IMPLEMENTATION_VERSION
  })
}

function initFragments(host: CliHost): readonly { readonly fileName: string; readonly contents: string }[] {
  const version = UNIFIED_BLE_IMPLEMENTATION_VERSION
  if (host === 'tauri') {
    return [
      {
        fileName: 'Cargo.toml.fragment',
        contents: [
          `# Generated by ubm init --host tauri (${version})`,
          `# Documented install: cargo add tauri-plugin-unified-ble-manager@4.0.0`,
          `# The crate is not yet published; until then use the checkout path fallback.`,
          `[dependencies]`,
          `tauri-plugin-unified-ble-manager = "4.0.0"`,
          ''
        ].join('\n')
      }
    ]
  }
  if (host === 'expo') {
    return [
      {
        fileName: 'app.json.fragment',
        contents: `${JSON.stringify(
          {
            expo: {
              plugins: [['unified-ble-manager', { requiredHardware: true }]]
            }
          },
          null,
          2
        )}\n`
      },
      {
        fileName: 'expo-factory.fragment.ts',
        contents: [
          `// Generated by ubm init --host expo (${version})`,
          '// Expo Go is not a supported BLE host. Use a development build. See docs/EXPO_PLUGIN.md',
          "import { createExpoBleManager } from 'unified-ble-manager/expo'",
          '',
          'const manager = await createExpoBleManager()',
          ''
        ].join('\n')
      }
    ]
  }
  if (host === 'electron') {
    return [
      {
        fileName: 'electron-renderer.fragment.ts',
        contents: [
          `// Generated by ubm init --host electron (${version})`,
          '// Main owns the radio via unified-ble-manager/electron/main.',
          '// The renderer never loads a Node-API addon.',
          "import { createElectronRendererBleManager } from 'unified-ble-manager/electron/renderer'",
          ''
        ].join('\n')
      }
    ]
  }
  if (host === 'web') {
    return [
      {
        fileName: 'web-chooser.fragment.ts',
        contents: [
          `// Generated by ubm init --host web (${version})`,
          "import { createWebBleManager } from 'unified-ble-manager/web'",
          '',
          'const manager = await createWebBleManager()',
          ''
        ].join('\n')
      }
    ]
  }
  return [
    {
      fileName: 'node-factory.fragment.ts',
      contents: [
        `// Generated by ubm init --host node (${version})`,
        '// Select one explicit factory. The root package does not choose a radio.',
        "import { createCoreBluetoothBleManager } from 'unified-ble-manager/node/corebluetooth'",
        "import { createWinRtBleManager } from 'unified-ble-manager/node/winrt'",
        "import { createBluezBleManager } from 'unified-ble-manager/node/bluez'",
        ''
      ].join('\n')
    }
  ]
}

async function runDoctorCommand(definition: CliBackendDefinition): Promise<UnifiedBleCliResult> {
  const adapters = await definition.factory.provider.listAdapters()
  return success('doctor', {
    backendId: definition.metadata.backendId,
    platformId: definition.metadata.platformId,
    providerId: definition.factory.provider.descriptor.providerId,
    hostKind: definition.factory.provider.descriptor.hostKind,
    loadability: definition.factory.provider.descriptor.loadability,
    selectedAdapterId: String(definition.factory.selection.selectedAdapterId),
    adapters: adapters.map(adapter => ({
      adapterId: String(adapter.adapterId),
      displayName: adapter.displayName,
      availability: adapter.state.availability,
      authorization: adapter.state.authorization,
      power: adapter.state.power,
      safeReason: adapter.state.safeReason,
      limitations: [...adapter.limitations]
    }))
  })
}

async function runCapabilitiesCommand(definition: CliBackendDefinition): Promise<UnifiedBleCliResult> {
  const fixture = await definition.factory.create(
    Object.freeze({ scenarioId: 'capability.truth-limits-evidence-and-binding' })
  )
  try {
    return success('capabilities', capabilityReportData(inspectBackendCapabilities(fixture.backend)))
  } finally {
    await fixture.dispose()
  }
}

async function runTckCommand(definition: CliBackendDefinition): Promise<UnifiedBleCliResult> {
  const report = await runBackendAuthorTck(definition)
  return success('tck', tckReportData(report))
}

async function runScenarioCommand(
  definition: CliBackendDefinition,
  scenarioId: string | null
): Promise<UnifiedBleCliResult> {
  if (scenarioId === null) {
    throw new UnifiedBleCliFailureError('cli.argument-invalid', 'scenario requires --scenario <id>')
  }
  const requestedScenario = baseTckScenarios.find(candidate => candidate.id === scenarioId)
  if (requestedScenario === undefined) {
    throw new UnifiedBleCliFailureError('cli.argument-invalid', `TCK scenario is not registered: ${scenarioId}`)
  }
  const report = await runBackendAuthorTck(definition)
  const receipt = report.receipts.find(candidate => candidate.scenarioId === requestedScenario.id)
  if (receipt === undefined) {
    throw new UnifiedBleCliFailureError(
      'cli.execution-failed',
      `selected backend did not execute applicable scenario ${requestedScenario.id}`
    )
  }
  return success('scenario', {
    scenarioId: requestedScenario.id,
    receipt: tckReceiptData(receipt),
    verification: report.verification,
    proofScope: report.proofScope
  })
}

async function runTraceCommand(
  invocation: ParsedTraceInvocation,
  runtime: UnifiedBleCliRuntime
): Promise<UnifiedBleCliResult> {
  let input: SerializableValue
  try {
    input = parseCliJson(await runtime.readTextFile(invocation.tracePath))
  } catch (error) {
    const message = error instanceof Error ? error.message : 'trace input failed without an Error value'
    return failure('trace', 'cli.trace-invalid', message)
  }
  if (invocation.action === 'redact') {
    try {
      const trace = redactTraceDocument(input)
      return success('trace', { action: 'redact', trace })
    } catch (error) {
      const message = error instanceof Error ? error.message : 'trace redaction failed without an Error value'
      return failure('trace', 'cli.trace-invalid', message)
    }
  }
  const validation = validateTraceDocument(input)
  return traceValidationResult(validation)
}

function traceValidationResult(validation: TraceValidationResult): UnifiedBleCliResult {
  const data = {
    valid: validation.valid,
    failures: validation.failures.map(item => ({ path: item.path, reason: item.reason }))
  }
  if (validation.valid) {
    return success('trace', data)
  }
  return {
    ok: false,
    command: 'trace',
    data,
    failures: [
      {
        code: 'cli.trace-invalid',
        message: `trace format validation failed with ${validation.failures.length} violation(s)`
      }
    ]
  }
}

function capabilityReportData(report: BackendCapabilityReport): SerializableRecord {
  return {
    backendId: report.backendId,
    platformId: report.platformId,
    capabilities: report.capabilities.map(capability => ({
      id: capability.id,
      state: capability.state,
      selectedSchemaRange: {
        minimum: capability.selectedSchemaMinimum,
        maximum: capability.selectedSchemaMaximum
      },
      implementationOrigin: capability.implementationOrigin,
      tck: {
        suiteId: capability.tck.suiteId,
        requiredScenarioIds: [...capability.tck.requiredScenarioIds],
        contractMinimum: capability.tck.contractMinimum,
        contractMaximum: capability.tck.contractMaximum
      },
      evidence: {
        verification: capability.evidence.verification,
        receiptId: capability.evidence.receiptId,
        evidenceLevel: capability.evidence.evidenceLevel,
        implementationVersion: capability.evidence.implementationVersion,
        sourceDigest: capability.evidence.sourceDigest,
        scenarioIds: [...capability.evidence.scenarioIds]
      },
      limitations: capability.limitations.map(limitation => ({
        code: limitation.code,
        explanation: limitation.explanation,
        affectedGuarantee: limitation.affectedGuarantee
      })),
      limits: capabilityLimitsData(capability.limits)
    }))
  }
}

function tckReportData(report: Awaited<ReturnType<typeof runBackendAuthorTck>>): SerializableRecord {
  return {
    backendId: report.backendId,
    verification: report.verification,
    proofScope: report.proofScope,
    identity: {
      registeredBackendId: report.identity.registeredBackendId,
      registeredPlatformId: report.identity.registeredPlatformId,
      providerId: report.identity.providerId,
      hostKind: report.identity.hostKind,
      implementationVersion: report.identity.implementationVersion,
      selectedAdapterId: report.identity.selectedAdapterId
    },
    baseScenarioIds: [...report.baseScenarioIds],
    featureSuiteIds: [...report.featureSuiteIds],
    featureBindings: report.featureBindings.map(binding => ({
      featureId: binding.featureId,
      state: binding.state,
      selectedSchemaRange: {
        minimum: binding.selectedSchemaMinimum,
        maximum: binding.selectedSchemaMaximum
      },
      implementationOrigin: binding.implementationOrigin,
      suiteId: binding.suiteId,
      requiredScenarioIds: [...binding.requiredScenarioIds],
      contractMinimum: binding.contractMinimum,
      contractMaximum: binding.contractMaximum,
      evidenceVerification: binding.evidenceVerification,
      receiptId: binding.receiptId,
      evidenceLevel: binding.evidenceLevel,
      implementationVersion: binding.implementationVersion,
      sourceDigest: binding.sourceDigest,
      evidenceScenarioIds: [...binding.evidenceScenarioIds],
      limitations: binding.limitations.map(limitation => ({
        code: limitation.code,
        explanation: limitation.explanation,
        affectedGuarantee: limitation.affectedGuarantee
      })),
      limits: capabilityLimitsData(binding.limits)
    })),
    receipts: report.receipts.map(tckReceiptData)
  }
}

function capabilityLimitsData(limits: CapabilityLimits): SerializableRecord {
  const data: Record<string, SerializableValue> = {}
  for (const [name, limit] of Object.entries(limits)) {
    data[name] = Object.freeze({ maximum: limit.maximum, minimum: limit.minimum, unit: limit.unit })
  }
  return Object.freeze(data)
}

function tckReceiptData(
  receipt: Awaited<ReturnType<typeof runBackendAuthorTck>>['receipts'][number]
): SerializableRecord {
  return {
    scenarioId: receipt.scenarioId,
    proof: {
      scope: receipt.proof.scope,
      claim: receipt.proof.claim,
      receiptId: receipt.proof.receiptId
    },
    facts: receipt.facts.map(fact => ({ id: fact.id, holds: fact.holds, detail: fact.detail })),
    error:
      receipt.error === null
        ? null
        : {
            code: receipt.error.code,
            domain: receipt.error.domain,
            operation: receipt.error.operation
          }
  }
}

function success(command: UnifiedBleCliCommand, data: SerializableRecord): UnifiedBleCliResult {
  return { ok: true, command, data, failures: [] }
}

function consumerSuccess(command: UnifiedBleCliCommand, data: SerializableRecord): UnifiedBleCliResult {
  return success(command, { schemaVersion: 1, ...data })
}

function failure(
  command: UnifiedBleCliCommand | null,
  code: UnifiedBleCliFailure['code'],
  message: string
): UnifiedBleCliResult {
  return { ok: false, command, data: null, failures: [{ code, message }] }
}

function isCliCommand(value: string | undefined): value is UnifiedBleCliCommand {
  return (
    value === 'doctor' ||
    value === 'capabilities' ||
    value === 'trace' ||
    value === 'tck' ||
    value === 'scenario' ||
    value === 'init' ||
    value === 'inspect' ||
    value === 'support-bundle'
  )
}

function parseCliHost(value: string | undefined): CliHost | UnifiedBleCliFailureError {
  if (value === 'expo' || value === 'tauri' || value === 'electron' || value === 'node' || value === 'web') {
    return value
  }
  return new UnifiedBleCliFailureError('cli.argument-invalid', '--host must be expo|tauri|electron|node|web', 'inspect')
}

function parseInspectInvocation(
  argumentsValue: readonly string[]
):
  | ParsedInspectPackageInvocation
  | ParsedInspectConfigInvocation
  | ParsedInspectCapabilitiesInvocation
  | UnifiedBleCliFailureError {
  const topic = argumentsValue[0]
  if (topic === 'package' && argumentsValue.length === 1) {
    return { kind: 'inspect-package', command: 'inspect' }
  }
  if (topic === 'config' || topic === 'capabilities') {
    if (argumentsValue[1] !== '--host') {
      return new UnifiedBleCliFailureError(
        'cli.argument-invalid',
        `inspect ${topic} requires --host expo|tauri|electron|node|web`,
        'inspect'
      )
    }
    const host = parseCliHost(argumentsValue[2])
    if (host instanceof UnifiedBleCliFailureError) {
      return host
    }
    if (argumentsValue.length !== 3) {
      return new UnifiedBleCliFailureError('cli.argument-invalid', `inspect ${topic} accepts --host only`, 'inspect')
    }
    return {
      kind: topic === 'config' ? 'inspect-config' : 'inspect-capabilities',
      command: 'inspect',
      host
    }
  }
  return new UnifiedBleCliFailureError(
    'cli.argument-invalid',
    'inspect requires package, config --host <host>, or capabilities --host <host>',
    'inspect'
  )
}

function parseSupportBundleInvocation(
  argumentsValue: readonly string[]
): ParsedSupportBundleInvocation | UnifiedBleCliFailureError {
  if (argumentsValue[0] !== 'create') {
    return new UnifiedBleCliFailureError(
      'cli.argument-invalid',
      'support-bundle requires create --output <path>',
      'support-bundle'
    )
  }
  let output: string | undefined
  let force = false
  for (let index = 1; index < argumentsValue.length; index += 1) {
    const token = argumentsValue[index]
    if (token === '--force') {
      force = true
      continue
    }
    if (token === '--output') {
      const value = argumentsValue[index + 1]
      if (value === undefined || value.startsWith('--') || value.length === 0) {
        return new UnifiedBleCliFailureError('cli.argument-invalid', '--output requires a value', 'support-bundle')
      }
      output = value
      index += 1
      continue
    }
    return new UnifiedBleCliFailureError(
      'cli.argument-invalid',
      `support-bundle does not accept ${token}`,
      'support-bundle'
    )
  }
  if (output === undefined) {
    return new UnifiedBleCliFailureError(
      'cli.argument-invalid',
      'support-bundle create requires --output <path>',
      'support-bundle'
    )
  }
  return { kind: 'support-bundle', command: 'support-bundle', output, force }
}

function parseInitInvocation(argumentsValue: readonly string[]): ParsedInitInvocation | UnifiedBleCliFailureError {
  let host: ParsedInitInvocation['host'] | undefined
  let directory: string | null = null
  let force = false
  for (let index = 0; index < argumentsValue.length; index += 1) {
    const token = argumentsValue[index]
    if (token === '--force') {
      force = true
      continue
    }
    if (token === '--host' || token === '--dir') {
      const value = argumentsValue[index + 1]
      if (value === undefined || value.startsWith('--') || value.length === 0) {
        return new UnifiedBleCliFailureError('cli.argument-invalid', `${token} requires a value`, 'init')
      }
      if (token === '--dir') directory = value
      else if (value === 'expo' || value === 'tauri' || value === 'electron' || value === 'node' || value === 'web') {
        host = value
      } else {
        return new UnifiedBleCliFailureError(
          'cli.argument-invalid',
          'init --host must be expo|tauri|electron|node|web',
          'init'
        )
      }
      index += 1
      continue
    }
    return new UnifiedBleCliFailureError('cli.argument-invalid', `init does not accept ${token}`, 'init')
  }
  if (host === undefined) {
    return new UnifiedBleCliFailureError(
      'cli.argument-invalid',
      'init requires --host expo|tauri|electron|node|web',
      'init'
    )
  }
  return { kind: 'init', command: 'init', host, directory, force }
}

class UnifiedBleCliFailureError extends Error {
  constructor(
    readonly code: UnifiedBleCliFailure['code'],
    message: string,
    readonly command: UnifiedBleCliCommand | null = null
  ) {
    super(message)
    this.name = 'UnifiedBleCliFailureError'
  }
}

const defaultCliRuntime: UnifiedBleCliRuntime = {
  readTextFile: async path => {
    const metadata = await stat(path)
    if (!metadata.isFile()) {
      throw new Error(`trace input is not a regular file: ${path}`)
    }
    if (metadata.size > maximumTraceFileBytes) {
      throw new Error(`trace input exceeds ${maximumTraceFileBytes} bytes: ${path}`)
    }
    return readFile(path, 'utf8')
  },
  loadBackendModule: async moduleSpecifier => {
    const filePath = filePathFromModuleSpecifier(moduleSpecifier)
    const requireSpecifier = filePath ?? moduleSpecifier
    if (!isExplicitEsModuleFile(requireSpecifier)) {
      try {
        const required: { readonly unifiedBleBackend?: CliBackendDefinition } =
          requireFromCliWorkingDirectory(requireSpecifier)
        return requiredBackendDefinition(required)
      } catch (error) {
        if (!isEsModuleRequireError(error)) {
          throw error
        }
      }
    }
    const importSpecifier = dynamicImportSpecifier(moduleSpecifier, filePath)
    try {
      const imported = await importBackendModule(importSpecifier)
      return requiredBackendDefinition(imported)
    } catch (error) {
      const detail = error instanceof Error ? error.message : String(error)
      throw new Error(`ES module import failed for ${importSpecifier}: ${detail}`, { cause: error })
    }
  },
  writeTextFile: async (path, contents) => {
    await writeFile(path, contents, 'utf8')
  },
  cwd: () => process.cwd()
}

function isRelativeOrAbsolutePath(moduleSpecifier: string): boolean {
  return moduleSpecifier.startsWith('.') || isAbsolute(moduleSpecifier)
}

function filePathFromModuleSpecifier(moduleSpecifier: string): string | null {
  if (!moduleSpecifier.startsWith('file:')) {
    return null
  }
  const url = new URL(moduleSpecifier)
  if (url.protocol !== 'file:') {
    throw new Error(`backend module URL must use the file protocol: ${moduleSpecifier}`)
  }
  return fileURLToPath(moduleSpecifier)
}

function dynamicImportSpecifier(moduleSpecifier: string, filePath: string | null): string {
  if (filePath !== null) {
    return pathToFileURL(filePath).href
  }
  if (isRelativeOrAbsolutePath(moduleSpecifier)) {
    return pathToFileURL(resolve(process.cwd(), moduleSpecifier)).href
  }
  return moduleSpecifier
}

function isExplicitEsModuleFile(moduleSpecifier: string): boolean {
  return moduleSpecifier.toLowerCase().endsWith('.mjs')
}

/**
 * Keeps Node's native `import()` available when a CommonJS test runner has
 * transformed this source file. The function body is static and never embeds a
 * backend-supplied specifier; the specifier remains a dynamic-import argument.
 */
async function importBackendModule(
  importSpecifier: string
): Promise<{ readonly unifiedBleBackend?: CliBackendDefinition }> {
  // eslint-disable-next-line no-new-func -- The static expression preserves Node import() under transformed CommonJS tests.
  const nativeDynamicImport = Function('specifier', 'return import(specifier)')
  const imported: unknown = nativeDynamicImport(importSpecifier)
  if (!isPromiseLike(imported)) {
    throw new Error('native dynamic import did not return a promise')
  }
  let namespace: unknown
  try {
    namespace = await imported
  } catch (error) {
    const detail = error instanceof Error ? error.message : String(error)
    throw new Error(`native dynamic import failed: ${detail}`, { cause: error })
  }
  if (!isBackendModuleNamespace(namespace)) {
    throw new Error('dynamic import did not return a module namespace')
  }
  return namespace
}

function isPromiseLike(value: unknown): value is PromiseLike<unknown> {
  return (
    value !== null &&
    (typeof value === 'object' || typeof value === 'function') &&
    'then' in value &&
    typeof value.then === 'function'
  )
}

function isBackendModuleNamespace(value: unknown): value is { readonly unifiedBleBackend?: CliBackendDefinition } {
  return value !== null && typeof value === 'object'
}

function isEsModuleRequireError(error: unknown): boolean {
  return error !== null && typeof error === 'object' && 'code' in error && error.code === 'ERR_REQUIRE_ESM'
}

function requiredBackendDefinition(moduleNamespace: {
  readonly unifiedBleBackend?: CliBackendDefinition
}): CliBackendDefinition {
  const definition = moduleNamespace.unifiedBleBackend
  if (definition === undefined) {
    throw new Error('module must export unifiedBleBackend')
  }
  return definition
}

export function formatUnifiedBleCliResult(result: UnifiedBleCliResult): string {
  return `${JSON.stringify(result)}\n`
}

export { redactTraceDocument, validateTraceDocument }
export type { DiagnosticTraceDocument, TraceValidationResult }

export function extractRedactedTrace(result: UnifiedBleCliResult): DiagnosticTraceDocument | null {
  if (result.command !== 'trace' || result.data === null || result.data.action !== 'redact') {
    return null
  }
  const trace = result.data.trace
  if (trace === undefined || typeof trace !== 'object' || trace === null || Array.isArray(trace)) {
    return null
  }
  return validateTraceDocument(trace).valid ? redactTraceDocument(trace) : null
}
