import { spawn } from 'node:child_process'
import { createInterface } from 'node:readline'
import { createNativeContinuationRecordingController } from '../../src/core/continuation-recording'
import { awaitSignal } from '../helpers/async'

let fixtureExecutable: string | undefined

beforeAll(async () => {
  const compiler = spawn(
    'cargo',
    ['test', '-p', 'ubm-mobile', '--test', 'recording_peer_scope', '--no-run', '--message-format=json'],
    {
      cwd: process.cwd(),
      stdio: ['ignore', 'pipe', 'pipe']
    }
  )
  let stderr = ''
  let parseFailure: Error | undefined
  compiler.stderr.on('data', chunk => {
    stderr = `${stderr}${String(chunk)}`.slice(-16000)
  })
  const lines = createInterface({ input: compiler.stdout })
  lines.on('line', line => {
    try {
      const message: unknown = JSON.parse(line)
      if (typeof message !== 'object' || message === null || Reflect.get(message, 'reason') !== 'compiler-artifact')
        return
      const target: unknown = Reflect.get(message, 'target')
      const executable: unknown = Reflect.get(message, 'executable')
      if (
        typeof target === 'object' &&
        target !== null &&
        Reflect.get(target, 'name') === 'recording_peer_scope' &&
        typeof executable === 'string'
      ) {
        fixtureExecutable = executable
      }
    } catch (error) {
      parseFailure = error instanceof Error ? error : new Error(String(error))
      compiler.kill()
    }
  })
  const closed = new Promise<void>((resolve, reject) => {
    compiler.on('error', reject)
    compiler.on('close', code => {
      if (parseFailure) reject(parseFailure)
      else if (code !== 0) reject(new Error(`mobile recording fixture compilation exited ${code}: ${stderr}`))
      else if (fixtureExecutable === undefined) reject(new Error('Cargo emitted no mobile recording test executable'))
      else resolve()
    })
  })
  try {
    await awaitSignal(closed, 'mobile recording fixture compilation', 180000)
  } catch (error) {
    compiler.kill()
    await awaitSignal(
      closed.catch(() => undefined),
      'mobile recording compiler termination',
      5000
    )
    throw error
  } finally {
    lines.close()
  }
}, 190000)

/** Real Rust MobileHost + injected radio + SQLite, not copied source excerpts. */
test.each(['android', 'apple'].flatMap(platform => [1, 2, 3].map(trial => ({ platform, trial }))))(
  'mobile $platform recording trial $trial excludes foreign-peer controls while ordinary delivery survives',
  async ({ platform }) => {
    if (fixtureExecutable === undefined) throw new Error('mobile recording fixture not compiled')
    const child = spawn(
      fixtureExecutable,
      ['mobile_recording_controller_transport', '--nocapture', '--test-threads=1'],
      {
        cwd: process.cwd(),
        env: { ...process.env, UBM_RECORDING_PLATFORM: platform, UBM_RECORDING_CONTROLLER_BRIDGE: '1' },
        stdio: ['pipe', 'pipe', 'pipe']
      }
    )
    let stderr = ''
    child.stderr.on('data', chunk => {
      stderr = `${stderr}${String(chunk)}`.slice(-16000)
    })
    const pending: Array<{ resolve(value: unknown): void; reject(error: Error): void }> = []
    const queued: unknown[] = []
    let terminated: Error | undefined
    const rejectPending = (failure: Error) => {
      terminated = failure
      for (const waiter of pending.splice(0)) waiter.reject(failure)
    }
    child.stdin.on('error', rejectPending)
    const lines = createInterface({ input: child.stdout })
    lines.on('line', line => {
      const prefix = 'UBM_RECORDING '
      // libtest's single-thread runner prefixes its first stdout line with
      // "test name ... "; subsequent bridge frames start at column zero.
      const offset = line.indexOf(prefix)
      if (offset < 0) return
      let value: unknown
      try {
        value = JSON.parse(line.slice(offset + prefix.length))
      } catch (error) {
        rejectPending(error instanceof Error ? error : new Error(String(error)))
        child.kill()
        return
      }
      const waiter = pending.shift()
      if (waiter) waiter.resolve(value)
      else queued.push(value)
    })
    const exit = new Promise<void>((resolve, reject) => {
      child.on('error', error => {
        rejectPending(error)
        reject(error)
      })
      child.on('close', code => {
        const failure = new Error(`mobile recording fixture exited ${code}: ${stderr}`)
        rejectPending(failure)
        if (code === 0) resolve()
        else reject(failure)
      })
    })
    // Attach a rejection handler even when the assertion fails before cleanup.
    const settled = exit.then(
      () => undefined,
      error => error
    )
    const receive = () => {
      if (queued.length > 0) return Promise.resolve(queued.shift())
      if (terminated !== undefined) return Promise.reject(terminated)
      return new Promise<unknown>((resolve, reject) => pending.push({ resolve, reject }))
    }
    const next = () => awaitSignal(receive(), 'mobile recording fixture response', 110000)
    const request = (op: string, extra = {}) => {
      child.stdin.write(`${JSON.stringify({ op, ...extra })}\n`)
      return next()
    }
    const native = async (op: string, extra = {}) => JSON.stringify(await request(op, extra))
    try {
      const ready = await next()
      expect(ready).toEqual(
        expect.objectContaining({
          ready: true,
          events: expect.arrayContaining([
            expect.objectContaining({ t: 'security', peerId: 'A0:9E:1A:00:00:02' }),
            expect.objectContaining({ t: 'db-changed', peerId: 'A0:9E:1A:00:00:02' }),
            expect.objectContaining({ t: 'link', peerId: 'A0:9E:1A:00:00:02' })
          ])
        })
      )
      const controller = createNativeContinuationRecordingController(
        {
          status: () => native('status'),
          prepare: (_, maxItems, maxBytes) => native('prepare', { maxItems, maxBytes }),
          acknowledge: (_, token) => native('acknowledge', { token }),
          stop: () => native('stop'),
          clear: () => native('clear')
        },
        'react-native'
      )
      const batch = await controller.prepare('peer-scope', { maxItems: 2, maxBytes: 65536 })
      expect(batch.token).not.toBeNull()
      expect((await controller.status('peer-scope')).lostRecords).toBe(0)
      await request('restart')
      expect(await controller.prepare('peer-scope', { maxItems: 2, maxBytes: 65536 })).toEqual(batch)
      const records = batch.records.slice()
      let current = batch
      let prefixes = 0
      while (current.token !== null) {
        expect((await controller.acknowledge('peer-scope', current.token)).records).toBe(current.records.length)
        prefixes += 1
        current = await controller.prepare('peer-scope', { maxItems: 2, maxBytes: 65536 })
        records.push(...current.records)
      }
      expect(prefixes).toBeGreaterThan(1)
      expect(records.every(row => row.metadata.session.peerId === 'A0:9E:1A:00:00:01')).toBe(true)
      expect(records.every(row => !('peerId' in row.record) || row.record.peerId === 'A0:9E:1A:00:00:01')).toBe(true)
      expect(
        records
          .filter(row => row.record.t === 'value')
          .map(row => {
            if (row.record.t !== 'value') throw new Error('value narrowing failed')
            return Array.from(row.record.value)
          })
      ).toEqual([
        [0, 70],
        [0, 71],
        [0, 72],
        [255, 0]
      ])
      expect(records.some(row => row.record.t === 'adapter')).toBe(true)
      expect((await controller.status('peer-scope')).records).toBe(0)
    } finally {
      child.stdin.end(`${JSON.stringify({ op: 'finish' })}\n`)
      let failure: unknown
      try {
        failure = await awaitSignal(settled, 'mobile recording fixture exit', 10000)
      } catch (error) {
        child.kill()
        await awaitSignal(settled, 'mobile recording fixture termination', 5000)
        failure = error
      }
      lines.close()
      if (failure !== undefined) throw failure
    }
  },
  120000
)
