import { test } from 'node:test'
import assert from 'node:assert/strict'
import { spawn, spawnSync } from 'node:child_process'
import { fileURLToPath } from 'node:url'
import { createHub } from '../../examples-shared/driver/server/hub.mjs'

test('no-radio CLI stop keeps every stdout line JSON and diagnostics on stderr', () => {
  const child = spawnSync(
    process.execPath,
    [
      '--disable-warning=MODULE_TYPELESS_PACKAGE_JSON',
      fileURLToPath(new URL('../driver.ts', import.meta.url)),
      'run',
      'live-dashboard',
      'stop',
      '{}'
    ],
    { encoding: 'utf8', timeout: 10000 }
  )
  assert.equal(child.error, undefined)
  assert.equal(child.status, 0, child.stderr)
  const records = child.stdout
    .trim()
    .split('\n')
    .map(line => JSON.parse(line))
  assert.ok(records.some(record => record.type === 'result' && record.command === 'stop'))
  assert.ok(records.some(record => record.type === 'shutdown-stop-all'))
  assert.match(child.stderr, /\[driver:live-dashboard\].*command/)
  assert.match(child.stderr, /"command":"stop"/)
  assert.doesNotMatch(child.stdout, /\[driver:/)
})

test('large no-radio rejected command flushes complete JSONL and stderr before explicit exit', () => {
  const padding = 'x'.repeat(120000)
  const child = spawnSync(
    process.execPath,
    [
      '--disable-warning=MODULE_TYPELESS_PACKAGE_JSON',
      fileURLToPath(new URL('../driver.ts', import.meta.url)),
      'run',
      'live-dashboard',
      'stop',
      JSON.stringify({ padding })
    ],
    { encoding: 'utf8', timeout: 10000, maxBuffer: 2000000 }
  )
  assert.equal(child.error, undefined)
  assert.equal(child.status, 1)
  const records = child.stdout
    .trim()
    .split('\n')
    .map(line => JSON.parse(line))
  assert.equal(records[0].event.data.args.padding, padding)
  assert.ok(records.some(record => record.type === 'error'))
  assert.equal(records.at(-1).type, 'shutdown-process-host')
  assert.deepEqual(records.at(-1).receipt, { state: 'released', failures: [] })
  assert.ok(child.stderr.includes(padding))
  assert.match(child.stderr, /command-failed/)
})

test(
  'server shutdown flushes its final JSON receipt and exits without native acquisition',
  { timeout: 10000 },
  async () => {
    const hub = createHub({ host: '127.0.0.1', port: 0 })
    const address = await hub.listen()
    const child = spawn(
      process.execPath,
      [
        '--disable-warning=MODULE_TYPELESS_PACKAGE_JSON',
        fileURLToPath(new URL('../driver.ts', import.meta.url)),
        'serve-host',
        '--driver-url',
        `ws://127.0.0.1:${address.port}/host`
      ],
      { stdio: ['ignore', 'pipe', 'pipe'] }
    )
    let stdout = '',
      stderr = '',
      signalled = false
    child.stdout.setEncoding('utf8').on('data', chunk => {
      stdout += chunk
    })
    child.stderr.setEncoding('utf8').on('data', chunk => {
      stderr += chunk
      if (!signalled && stderr.includes('[example-node] remote connected')) {
        signalled = true
        child.kill('SIGINT')
      }
    })
    try {
      const status = await new Promise((resolve, reject) => {
        child.once('error', reject)
        child.once('close', resolve)
      })
      assert.equal(status, 0, stderr)
      const records = stdout
        .trim()
        .split('\n')
        .map(line => JSON.parse(line))
      assert.equal(records.at(-1).type, 'shutdown-process-host')
      assert.deepEqual(records.at(-1).receipt, { state: 'released', failures: [] })
    } finally {
      if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL')
      await hub.close()
    }
  }
)
