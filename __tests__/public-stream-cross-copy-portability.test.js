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
    const output = error instanceof Error && 'output' in error && Array.isArray(error.output)
      ? error.output.map(value => String(value ?? '')).join('')
      : ''
    throw new Error(`cross-copy declaration compilation failed:\n${stdout}${stderr}${output}`)
  }
}

describe('public stream cross-copy portability', () => {
  test('assigns every root manager and stream surface across separately packed copies', () => {
    expect(() => compileCrossCopyFixture()).not.toThrow()
  })
})
