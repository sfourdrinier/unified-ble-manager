'use strict'

const fs = require('node:fs')
const path = require('node:path')
const vm = require('node:vm')
const ts = require('typescript')

const root = path.join(__dirname, '..')
const guide = path.join(root, 'docs/TAURI.md')

function frontendRecipe() {
  const markdown = fs.readFileSync(guide, 'utf8')
  const match = /^## Frontend\s+```ts\n([\s\S]*?)\n```/mu.exec(markdown)
  if (match === null) throw new Error('Tauri guide has no complete Frontend TypeScript recipe')
  return match[1]
}

function typecheckRecipe(source) {
  const configPath = ts.findConfigFile(root, ts.sys.fileExists, 'tsconfig.json')
  const config = ts.readConfigFile(configPath, ts.sys.readFile)
  if (config.error) throw new Error(ts.flattenDiagnosticMessageText(config.error.messageText, '\n'))
  const parsed = ts.parseJsonConfigFileContent(config.config, ts.sys, root)
  const virtualFile = path.join(root, '__tests__/tauri-frontend-guide.virtual.ts')
  // TypeScript normalizes compiler paths to forward slashes on Windows;
  // node:path joins with backslashes there. Compare filesystem identities.
  const isVirtualFile = fileName => path.normalize(fileName) === virtualFile
  const host = ts.createCompilerHost(parsed.options)
  const getSourceFile = host.getSourceFile.bind(host)
  const fileExists = host.fileExists.bind(host)
  const readFile = host.readFile.bind(host)
  host.getSourceFile = (fileName, languageVersion, onError, shouldCreateNewSourceFile) =>
    isVirtualFile(fileName)
      ? ts.createSourceFile(fileName, source, languageVersion, true)
      : getSourceFile(fileName, languageVersion, onError, shouldCreateNewSourceFile)
  host.fileExists = fileName => isVirtualFile(fileName) || fileExists(fileName)
  host.readFile = fileName => (isVirtualFile(fileName) ? source : readFile(fileName))
  const program = ts.createProgram([virtualFile], { ...parsed.options, noEmit: true }, host)
  const diagnostics = ts.getPreEmitDiagnostics(program)
  if (diagnostics.length > 0) {
    throw new Error(
      ts.formatDiagnosticsWithColorAndContext(diagnostics, {
        getCanonicalFileName: fileName => fileName,
        getCurrentDirectory: () => root,
        getNewLine: () => '\n'
      })
    )
  }
}

function executeRecipe(source, manager) {
  const importLine = /^import \{ createTauriBleManager \} from 'unified-ble-manager\/tauri'\s*/u
  if (!importLine.test(source)) throw new Error('Tauri recipe must import its public factory')
  const body = source.replace(importLine, '')
  const compiled = ts.transpileModule(body, { compilerOptions: { target: ts.ScriptTarget.ES2022 } }).outputText
  const run = vm.runInNewContext(`(async context => { const { createTauriBleManager } = context; ${compiled}\n})`, {
    AbortController,
    Error
  })
  return run({ createTauriBleManager: async () => manager })
}

function managerFixture(failAt = null) {
  const peer = { id: 'observed-peer' }
  const failure = new Error(`${failAt} failed`)
  const read = jest.fn(async () => {
    if (failAt === 'read') throw failure
    return new Uint8Array([73])
  })
  const characteristic = jest.fn(() => ({ read }))
  const connection = {
    discover: jest.fn(async () => {
      if (failAt === 'discover') throw failure
      return { characteristic }
    }),
    release: jest.fn(async () => {
      if (failAt === 'connection-release') throw failure
      return { state: 'released', failures: [] }
    })
  }
  const scan = {
    observations: {
      [Symbol.asyncIterator]: () => ({
        next: async () => {
          if (failAt === 'observation') throw failure
          return { done: false, value: { kind: 'value', value: { peer } } }
        }
      })
    },
    stop: jest.fn(async () => {
      if (failAt === 'scan-stop') throw failure
      return { state: 'released', failures: [] }
    })
  }
  const manager = {
    scan: jest.fn(async () => {
      if (failAt === 'scan') throw failure
      return scan
    }),
    connect: jest.fn(async () => {
      if (failAt === 'connect') throw failure
      return connection
    }),
    destroy: jest.fn(async () => ({ state: 'released', failures: [] }))
  }
  return { manager, scan, connection, characteristic, read, peer, failure }
}

describe('complete Tauri Frontend guide recipe', () => {
  const source = frontendRecipe()

  test('typechecks the exact documented TypeScript against the public package', () => {
    typecheckRecipe(source)
  })

  test('reads the nested scan value, connects that peer, and reads Battery Level', async () => {
    const fixture = managerFixture()
    await expect(executeRecipe(source, fixture.manager)).resolves.toBeUndefined()
    expect(fixture.manager.connect).toHaveBeenCalledWith(fixture.peer, expect.objectContaining({ timeoutMs: 10_000 }))
    expect(fixture.characteristic).toHaveBeenCalledWith('180f', '2a19')
    expect(fixture.read).toHaveBeenCalledTimes(1)
    expect(fixture.scan.stop).toHaveBeenCalledTimes(1)
    expect(fixture.connection.release).toHaveBeenCalledTimes(1)
    expect(fixture.manager.destroy).toHaveBeenCalledTimes(1)
  })

  test.each([
    ['scan', 0, 0],
    ['observation', 1, 0],
    ['scan-stop', 1, 0],
    ['connect', 1, 0],
    ['discover', 1, 1],
    ['read', 1, 1],
    ['connection-release', 1, 1]
  ])('cleans up when %s fails', async (stage, stops, releases) => {
    const fixture = managerFixture(stage)
    await expect(executeRecipe(source, fixture.manager)).rejects.toThrow(`${stage} failed`)
    expect(fixture.scan.stop).toHaveBeenCalledTimes(stops)
    expect(fixture.connection.release).toHaveBeenCalledTimes(releases)
    expect(fixture.manager.destroy).toHaveBeenCalledTimes(1)
  })
})
