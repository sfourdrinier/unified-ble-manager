import { test } from 'node:test'
import assert from 'node:assert/strict'
import fs from 'node:fs'
import vm from 'node:vm'
import ts from 'typescript'

test('browser boot forwards the exact optional native process authority without creating a manager', () => {
  const source = fs.readFileSync(new URL('../browser/boot.ts', import.meta.url), 'utf8')
  const compiled = ts.transpileModule(source, {
    compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 }
  }).outputText
  const nativeContinuation = { status: async () => null }
  let managerCalls = 0
  const createManager = async () => {
    managerCalls++
    throw new Error('no radio at boot')
  }
  const exports = {}
  let registeredHost
  const modules = {
    '../create-driver.ts': {
      createScenarioRegistry: host => {
        registeredHost = host
        return {}
      },
      createRemoteDriver: () => ({ start() {} })
    },
    '../host.ts': { hostLabel: () => 'test' },
    '../scenario-core.ts': { createConsoleRuntime: () => ({}) },
    '../user-gesture.ts': {},
    './host-facts.ts': {
      platformFromUserAgent: () => 'test',
      engineFromUserAgent: () => 'test',
      documentAppState: () => null
    },
    './panel.ts': { mountDriverPanel: () => () => {} }
  }
  vm.runInNewContext(compiled, {
    exports,
    navigator: { userAgent: 'test' },
    require: name => {
      assert.ok(Object.hasOwn(modules, name), name)
      return modules[name]
    }
  })
  const base = {
    host: 'electron',
    backend: 'test',
    createManager,
    requireUserGesture: false,
    driverUrl: {},
    mount: { ownerDocument: {} },
    appBuild: {}
  }
  const driver = exports.bootBrowserDriver({ ...base, nativeContinuation })
  assert.equal(driver.host.nativeContinuation, nativeContinuation)
  assert.equal(registeredHost, driver.host)
  assert.equal(driver.host.createManager, createManager)
  assert.equal(managerCalls, 0)
  assert.equal(
    exports.bootBrowserDriver(base).host.nativeContinuation,
    undefined,
    'ordinary Web host gains no native mechanism'
  )
})
