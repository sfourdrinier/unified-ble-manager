'use strict'

// Exact native binary + synthetic radio + real SQLite; never opens a radio.
// Each invocation owns fresh recordings. Process exit releases cached authorities.
const assert = require('node:assert/strict')
const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')
const crypto = require('node:crypto')
const { setImmediate: yieldTurn } = require('node:timers/promises')
const SERVICE = '0000180d-0000-1000-8000-00805f9b34fb'
const CHARACTERISTIC = '00002a37-0000-1000-8000-00805f9b34fb'
const MAX_BYTES = 4194304
const MAX_ITEMS = 2048
const ownedDirectories = []
const selector = {
  serviceUuid: SERVICE,
  serviceOccurrence: 1,
  characteristicUuid: CHARACTERISTIC,
  characteristicOccurrence: 1
}

function envelope(text) {
  return JSON.parse(text)
}
function unwrap(text) {
  const result = envelope(text)
  assert.equal(result.ok, true, JSON.stringify(result))
  return result.value
}
async function until(read, accept, label) {
  const deadline = Date.now() + 15000
  let last
  do {
    last = await read()
    if (accept(last)) return last
    await yieldTurn()
  } while (Date.now() < deadline)
  throw new Error(`${label}: ${JSON.stringify(last)}`)
}
async function stoppedIngress(ctx, id) {
  const previous = (await ctx.central.stagedGattAccesses()).length
  let earlyReply
  const execution = ctx.central.continuationExecute('peer', ctx.declaration(id, true)).then(text => {
    earlyReply = envelope(text)
    return text
  })
  // Native setup installs its observation BEFORE dispatching this write.
  // The write log fences setup admission; not a guessed time allowance.
  await until(
    async () => {
      if (earlyReply) {
        assert.equal(earlyReply.ok, true, JSON.stringify(earlyReply))
        throw new Error('setup finished before ingress')
      }
      return (await ctx.central.stagedGattAccesses()).length
    },
    count => count > previous,
    'setup write'
  )
  unwrap(await ctx.store.stop(id))
  await ctx.stage(Buffer.from([255, 0]))
  const reply = envelope(await execution)
  // Only stopped push_data drops this installed observer, before handoff.
  assert.equal(reply.ok, false)
  assert.equal(reply.error.code, 'stream.closed', JSON.stringify(reply))
  assert.equal(reply.error.detail, 'setup acknowledgement observation closed', JSON.stringify(reply))
}
function handles() {
  if (process.platform !== 'linux') return null
  return fs.readdirSync('/proc/self/fd').filter(fd => {
    try {
      const target = fs.readlinkSync(`/proc/self/fd/${fd}`)
      return (
        ownedDirectories.some(directory => target.startsWith(`${directory}${path.sep}`)) && target.includes('.sqlite')
      )
    } catch (error) {
      if (error.code === 'ENOENT') return false
      throw error
    }
  }).length
}
async function context(addon, label) {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), `ubm-retirement-${label}-`))
  ownedDirectories.push(directory)
  const central = await addon.UbmCentral.openSynthetic(label)
  await central.stageServices('peer', [
    {
      uuid: SERVICE,
      occurrence: 0,
      characteristics: [
        {
          uuid: CHARACTERISTIC,
          occurrence: 0,
          properties: { read: true, write: true, writeWithoutResponse: false, notify: true, indicate: false },
          descriptors: []
        }
      ]
    }
  ])
  await central.stageWriteLimits('peer', 512, 512)
  unwrap(await central.continuationConfigureRecordingDirectory(directory))
  const store = new addon.ContinuationRecordingStore()
  unwrap(await store.configureDirectory(directory))
  return {
    directory,
    central,
    store,
    declaration(id, setup = false) {
      return JSON.stringify({
        onAppearance: 'native',
        resubscribe: [selector],
        recording: { id, maxBytes: 16 * 1048576, maxRecords: 10000 },
        ...(setup
          ? {
              setup: [
                {
                  selector,
                  value: [2, 0],
                  timeoutMs: 15000,
                  response: {
                    subscriptionIndex: 0,
                    prefix: [255],
                    minLength: 2,
                    maxLength: 2,
                    status: { offset: 1, accepted: [0] }
                  }
                }
              ]
            }
          : {})
      })
    },
    stage(value) {
      return central.stageNotification({
        peerId: 'peer',
        serviceUuid: SERVICE,
        serviceOccurrence: 0,
        characteristicUuid: CHARACTERISTIC,
        characteristicOccurrence: 0,
        value
      })
    }
  }
}
async function retire(ctx, id) {
  const claim = unwrap(await ctx.central.continuationPrepareClaim(MAX_ITEMS, MAX_BYTES))
  const receipt = unwrap(await ctx.central.continuationAcknowledgeClaim(claim.claimToken))
  assert.equal(receipt.disposed, true)
  unwrap(await ctx.store.clear(id))
  const status = unwrap(await ctx.store.status(id))
  assert.equal(status.records, 0)
  assert.equal(status.bytes, 0)
  assert.equal(status.collectionFailure, null)
  return { status, claim }
}
async function retention(addon, mode, cycles, baseline) {
  const ctx = await context(addon, mode)
  let completed = 0
  try {
    for (let index = 0; index < cycles; index++) {
      const id = `r${index}`
      if (mode === 'independent-late') await stoppedIngress(ctx, id)
      else {
        unwrap(await ctx.central.continuationExecute('peer', ctx.declaration(id)))
        if (mode === 'engine-late') {
          unwrap(await ctx.central.continuationRecordingStop(id))
          await ctx.stage(Buffer.from([255, 0]))
        } else unwrap(await ctx.store.stop(id))
      }
      const { status, claim } = await retire(ctx, id)
      if (mode === 'independent-late') {
        assert.equal(claim.afterCutoffLoss.items, 1)
        if (baseline) assert.equal(status.runtimeFailure.kind, 'storage.stopped')
        else assert.equal(status.runtimeFailure, null)
      } else assert.equal(status.runtimeFailure, null)
      completed++
      const count = handles()
      if (!baseline && count !== null) assert(count <= 16, `inactive handles ${count} exceeds 16`)
    }
    return { mode, completed, journalHandles: handles(), directory: ctx.directory }
  } catch (error) {
    if (!baseline || completed !== 256 || !String(error).includes('authority capacity reached')) {
      throw new Error(`completed=${completed}: ${error.message}`, { cause: error })
    }
    return {
      mode,
      completed,
      blockedAt: 257,
      journalHandles: handles(),
      directory: ctx.directory,
      refusal: String(error)
    }
  } finally {
    assert.equal((await ctx.central.close()).state, 'released')
  }
}
async function exportsControl(addon, trials) {
  const results = []
  for (let trial = 0; trial < trials; trial++) {
    const ctx = await context(addon, `export-${trial}`)
    try {
      unwrap(await ctx.central.continuationExecute('peer', ctx.declaration('export')))
      for (let index = 0; index < 500; index++) await ctx.stage(Buffer.from([index % 256, 42]))
      await until(
        () => ctx.store.status('export').then(unwrap),
        status => status.records === 501,
        'seed committed'
      )
      const pending = ctx.store.prepare('export', 128, MAX_BYTES)
      for (let index = 500; index < 600; index++) await ctx.stage(Buffer.from([index % 256, 42]))
      const prefix = unwrap(await pending)
      const status = await until(
        () => ctx.store.status('export').then(unwrap),
        current => current.records === 601,
        'all values committed'
      )
      assert.equal(status.accepting, true)
      assert.equal(status.runtimeFailure, null)
      assert.equal(status.collectionFailure, null)
      assert.equal(status.lostRecords, 0)
      assert.deepEqual(unwrap(await ctx.store.prepare('export', 128, MAX_BYTES)), prefix)
      assert.equal(envelope(await ctx.store.acknowledge('export', 'wrong')).ok, false)
      const receipt = unwrap(await ctx.store.acknowledge('export', prefix.token))
      assert.deepEqual(unwrap(await ctx.store.acknowledge('export', prefix.token)), receipt)
      const rest = unwrap(await ctx.store.prepare('export', MAX_ITEMS, MAX_BYTES))
      const values = [...prefix.records, ...rest.records].filter(row => row.record.t === 'value')
      assert.equal(values.length, 600)
      assert.deepEqual(
        values.map(row => [...Buffer.from(row.record.valueB64, 'base64')]),
        Array.from({ length: 600 }, (_, index) => [index % 256, 42])
      )
      unwrap(await ctx.central.continuationRecordingStop('export'))
      await retire(ctx, 'export')
      results.push({ trial, records: status.records, immutablePrefix: true, exactValues: values.length })
    } finally {
      assert.equal((await ctx.central.close()).state, 'released')
    }
  }
  return results
}
async function main(env = process.env) {
  assert(env.UBM_NAPI_ADDON && path.isAbsolute(env.UBM_NAPI_ADDON), 'explicit absolute UBM_NAPI_ADDON required')
  const addon = require(env.UBM_NAPI_ADDON)
  const cycles = Number(env.CYCLES || 1000)
  assert(Number.isInteger(cycles) && cycles > 256 && cycles <= 2000)
  const baseline = env.EXPECT_RC14_BUG === '1'
  const identity = {
    binary: env.UBM_NAPI_ADDON,
    sha256: crypto.createHash('sha256').update(fs.readFileSync(env.UBM_NAPI_ADDON)).digest('hex'),
    synthetic: true
  }
  // Baseline exhaustion must start in a fresh process with no prior cache IDs.
  const exports = baseline ? [] : await exportsControl(addon, 3)
  const results = [await retention(addon, 'independent-late', cycles, baseline)]
  if (baseline) assert.equal(results[0].blockedAt, 257)
  else {
    results.push(await retention(addon, 'engine-late', cycles, false))
    results.push(await retention(addon, 'independent-no-ingress', cycles, false))
  }
  console.log(JSON.stringify({ identity, baseline, results, exports }, null, 2))
}
module.exports = { stoppedIngress, main }
if (require.main === module)
  main().catch(error => {
    console.error(error)
    process.exitCode = 1
  })
