// __tests__/public-stream-cross-copy-portability.test.js

const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')
const { execFileSync } = require('node:child_process')

const rootDirectory = path.join(__dirname, '..')

function compileCrossCopyFixture() {
  const packedDeclarations = path.join(rootDirectory, 'lib', 'typescript', 'commonjs')
  if (!fs.existsSync(packedDeclarations)) {
    throw new Error('packed declaration tree is missing; run pnpm prepack before this check')
  }

  const temporaryRoot = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-public-stream-cross-copy-'))
  const copyA = path.join(temporaryRoot, 'copy-a')
  const copyB = path.join(temporaryRoot, 'copy-b')
  fs.cpSync(packedDeclarations, copyA, { recursive: true })
  fs.cpSync(packedDeclarations, copyB, { recursive: true })
  const fixture = path.join(temporaryRoot, 'fixture.ts')
  fs.writeFileSync(
    fixture,
    `import type * as CopyA from './copy-a/src/index'
import type * as CopyB from './copy-b/src/index'

declare const managerA: CopyA.BleManager
declare const managerB: CopyB.BleManager
declare const connectionA: CopyA.BleConnection
declare const connectionB: CopyB.BleConnection
declare const databaseA: CopyA.GattDatabase
declare const databaseB: CopyB.GattDatabase
declare const subscriptionA: CopyA.GattSubscription
declare const subscriptionB: CopyB.GattSubscription
declare const scanA: CopyA.ScanSession
declare const scanB: CopyB.ScanSession
declare const adapterWatchA: Awaited<ReturnType<CopyA.BleAdapter['watchState']>>
declare const adapterWatchB: Awaited<ReturnType<CopyB.BleAdapter['watchState']>>
declare const supervisorA: CopyA.ConnectionSupervisor<{ readonly source: string }>
declare const supervisorB: CopyB.ConnectionSupervisor<{ readonly source: string }>
declare const diagnosticsA: CopyA.BleDiagnostics
declare const diagnosticsB: CopyB.BleDiagnostics
declare const errorA: CopyA.BleError
declare const errorB: CopyB.BleError
declare const platformDetailA: CopyA.PublicPlatformErrorDetail
declare const platformDetailB: CopyB.PublicPlatformErrorDetail

type TauriManagerA = Awaited<ReturnType<typeof import('./copy-a/src/tauri').createTauriBleManagerWithEnvironment>>
type TauriManagerB = Awaited<ReturnType<typeof import('./copy-b/src/tauri').createTauriBleManagerWithEnvironment>>
type ElectronManagerA = Awaited<ReturnType<typeof import('./copy-a/src/electron/public-manager').createElectronRendererBleManager>>
type ElectronManagerB = Awaited<ReturnType<typeof import('./copy-b/src/electron/public-manager').createElectronRendererBleManager>>

const managerFromA: CopyB.BleManager = managerA
const managerFromB: CopyA.BleManager = managerB
const connectionFromA: CopyB.BleConnection = connectionA
const connectionFromB: CopyA.BleConnection = connectionB
const databaseFromA: CopyB.GattDatabase = databaseA
const databaseFromB: CopyA.GattDatabase = databaseB
const subscriptionFromA: CopyB.GattSubscription = subscriptionA
const subscriptionFromB: CopyA.GattSubscription = subscriptionB
const scanFromA: CopyB.ScanSession = scanA
const scanFromB: CopyA.ScanSession = scanB
const adapterWatchFromA: Awaited<ReturnType<CopyB.BleAdapter['watchState']>> = adapterWatchA
const adapterWatchFromB: Awaited<ReturnType<CopyA.BleAdapter['watchState']>> = adapterWatchB
const supervisorFromA: CopyB.ConnectionSupervisor<{ readonly source: string }> = supervisorA
const supervisorFromB: CopyA.ConnectionSupervisor<{ readonly source: string }> = supervisorB
const diagnosticsFromA: CopyB.BleDiagnostics = diagnosticsA
const diagnosticsFromB: CopyA.BleDiagnostics = diagnosticsB
const errorFromA: CopyB.BleError = errorA
const errorFromB: CopyA.BleError = errorB
const platformDetailFromA: CopyB.PublicPlatformErrorDetail = platformDetailA
const platformDetailFromB: CopyA.PublicPlatformErrorDetail = platformDetailB
declare const tauriManagerA: TauriManagerA
declare const tauriManagerB: TauriManagerB
declare const electronManagerA: ElectronManagerA
declare const electronManagerB: ElectronManagerB
const tauriFromA: TauriManagerB = tauriManagerA
const tauriFromB: TauriManagerA = tauriManagerB
const electronFromA: ElectronManagerB = electronManagerA
const electronFromB: ElectronManagerA = electronManagerB

void managerFromA
void managerFromB
void connectionFromA
void connectionFromB
void databaseFromA
void databaseFromB
void subscriptionFromA
void subscriptionFromB
void scanFromA
void scanFromB
void adapterWatchFromA
void adapterWatchFromB
void supervisorFromA
void supervisorFromB
void diagnosticsFromA
void diagnosticsFromB
void errorFromA
void errorFromB
void platformDetailFromA
void platformDetailFromB
void tauriFromA
void tauriFromB
void electronFromA
void electronFromB
`,
    'utf8'
  )
  const config = path.join(temporaryRoot, 'tsconfig.json')
  fs.writeFileSync(
    config,
    JSON.stringify(
      {
        compilerOptions: {
          lib: ['esnext', 'dom'],
          module: 'esnext',
          moduleResolution: 'node',
          noEmit: true,
          noUncheckedIndexedAccess: true,
          noUnusedLocals: true,
          noUnusedParameters: true,
          strict: true,
          target: 'esnext',
          verbatimModuleSyntax: true
        },
        files: [fixture]
      },
      null,
      2
    ),
    'utf8'
  )
  const tsc = path.join(rootDirectory, 'node_modules', 'typescript', 'bin', 'tsc')
  try {
    return execFileSync(process.execPath, [tsc, '-p', config], {
      cwd: rootDirectory,
      encoding: 'utf8',
      stdio: 'pipe'
    })
  } catch (error) {
    const stderr = error instanceof Error && 'stderr' in error ? String(error.stderr) : ''
    const stdout = error instanceof Error && 'stdout' in error ? String(error.stdout) : ''
    const output =
      error instanceof Error && 'output' in error && Array.isArray(error.output)
        ? error.output.map(value => String(value ?? '')).join('')
        : ''
    throw new Error(`cross-copy declaration compilation failed:\n${stdout}${stderr}${output}`)
  }
}

describe('public stream cross-copy portability', () => {
  test('assigns every root manager and stream surface across separately packed copies', () => {
    expect(() => compileCrossCopyFixture()).not.toThrow()
  })

  test('the reviewed root API report describes only the public bounded stream contract', () => {
    const report = fs.readFileSync(path.join(rootDirectory, 'etc', 'api', 'root.api.md'), 'utf8')
    expect(report).not.toContain('readonly observations: BoundedAsyncStream<PublicScanObservation>')
    expect(report).toContain('readonly observations: PublicBoundedAsyncStream<PublicScanObservation>')
  })
})
