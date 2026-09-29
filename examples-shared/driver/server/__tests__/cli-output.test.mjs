import { test } from 'node:test'
import assert from 'node:assert/strict'
import { execFile, spawn } from 'node:child_process'
import { mkdtemp, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { promisify } from 'node:util'
import { fileURLToPath } from 'node:url'
import { createHub } from '../hub.mjs'
import { startNodeHost } from './node-host.mjs'

for (const signal of ['SIGINT', 'SIGTERM']) {
  test(`serve ${signal} closes owned sockets and exits naturally`, { timeout: 20000 }, async () => {
    const directory = await mkdtemp(join(tmpdir(), 'ubm-cli-shutdown-'))
    const child = spawn(process.execPath, [fileURLToPath(new URL('../cli.mjs', import.meta.url)), 'serve', '--host', '127.0.0.1', '--port', '0', '--log-dir', directory])
    const exit = new Promise((resolve, reject) => { child.once('close', (code, signal) => resolve({ code, signal })); child.once('error', reject) })
    let socket
    let output = ''
    child.stdout.on('data', chunk => { output += chunk })
    try {
      const port = await new Promise(resolve => {
        let diagnostic = ''
        child.stderr.on('data', chunk => {
          diagnostic += chunk
          const match = /listening on 127\.0\.0\.1:(\d+)/.exec(diagnostic)
          if (match) resolve(Number(match[1]))
        })
      })
      socket = new WebSocket(`ws://127.0.0.1:${port}/control`)
      await new Promise((resolve, reject) => { socket.addEventListener('open', resolve, { once: true }); socket.addEventListener('error', reject, { once: true }) })
      child.kill(signal)
      assert.deepEqual(await exit, { code: 0, signal: null })
      for (const line of output.trim().split('\n').filter(Boolean)) JSON.parse(line)
    } finally {
      socket?.close()
      if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL')
      await exit
      await rm(directory, { recursive: true })
    }
  })
}

test('CLI failure drains its complete diagnostic before returning the failure exit code', async () => {
  const unknown = 'unknown'.repeat(17000)
  await assert.rejects(promisify(execFile)(process.execPath, [fileURLToPath(new URL('../cli.mjs', import.meta.url)), unknown]), error => {
    assert.equal(error.code, 2)
    assert.ok(error.stderr.includes(`unknown command "${unknown}"`))
    return true
  })
})

test('CLI run drains complete multi-megabyte JSON to a pipe before exiting', async () => {
  let connected
  const ready = new Promise(resolve => { connected = resolve })
  const hub = createHub({ port: 0, host: '127.0.0.1', onRecord: record => {
    if (record.event === 'host-connected') connected()
  } })
  const { port } = await hub.listen()
  const host = startNodeHost({ port, platform: 'android' })
  const payload = '0123456789'.repeat(250000)
  host.registry.get('demo').commands.start.execute = async () => ({ payload })
  try {
    await ready
    const { stdout } = await promisify(execFile)(process.execPath, [
      fileURLToPath(new URL('../cli.mjs', import.meta.url)), 'run', 'all', 'demo', 'start', '{}',
      '--server', `ws://127.0.0.1:${port}/control`
    ], { maxBuffer: 32 * 1024 * 1024, timeout: 20000 })
    const lines = stdout.trim().split('\n').map(JSON.parse)
    const summary = lines.find(line => line.type === 'run-summary')
    assert.equal(summary.outcomes[0].ok, true)
    assert.equal(summary.outcomes[0].result.payload, payload)
  } finally {
    host.channel.stop()
    await hub.close()
  }
})
