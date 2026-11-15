const { Worker } = require('node:worker_threads')

// Run the production coordinator in independent JS realms using Node's Web
// Locks implementation. IndexedDB transaction behavior is covered separately.
const workerSource = `
  const { parentPort, workerData } = require('node:worker_threads')
  const fs = require('node:fs')
  const ts = require(workerData.typescript)
  require.extensions['.ts'] = (module, filename) => {
    const source = fs.readFileSync(filename, 'utf8')
    const compiled = ts.transpileModule(source, { compilerOptions: {
      target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.CommonJS
    } }).outputText
    module._compile(compiled, filename)
  }
  const { acquireWebRecordingClearLock } = require(workerData.coordinator)
  let release
  parentPort.on('message', async command => {
    if (command === 'probe') {
      parentPort.postMessage(typeof navigator !== 'undefined' && navigator.locks !== undefined)
    } else if (command === 'acquire') {
      try {
        release = await acquireWebRecordingClearLock(typeof navigator === 'undefined' ? undefined : navigator.locks)
        parentPort.postMessage('acquired')
      } catch (error) {
        parentPort.postMessage(error.normalized?.code ?? error.message)
      }
    } else if (command === 'release') {
      release()
      parentPort.postMessage('released')
    }
  })
`

function worker() {
  return new Worker(workerSource, {
    eval: true,
    workerData: {
      typescript: require.resolve('typescript'),
      coordinator: require.resolve('../../src/web/web-recording-clear-lock.ts')
    }
  })
}
function command(target, message) {
  return new Promise((resolve, reject) => {
    const received = value => {
      target.off('error', failed)
      resolve(value)
    }
    const failed = error => {
      target.off('message', received)
      reject(error)
    }
    target.once('message', received)
    target.once('error', failed)
    target.postMessage(message)
  })
}

test('independent worker realms share clear ownership and recover when the owner ends', async () => {
  const first = worker()
  const second = worker()
  try {
    const hasLocks = await command(first, 'probe')
    const firstResult = await command(first, 'acquire')
    if (!hasLocks) {
      expect(firstResult).toBe('capability.unsupported')
      expect(await command(second, 'acquire')).toBe('capability.unsupported')
      return
    }
    expect(firstResult).toBe('acquired')
    expect(await command(second, 'acquire')).toBe('capability.unavailable')
    await first.terminate()
    expect(await command(second, 'acquire')).toBe('acquired')
    expect(await command(second, 'release')).toBe('released')
  } finally {
    await first.terminate()
    await second.terminate()
  }
})
