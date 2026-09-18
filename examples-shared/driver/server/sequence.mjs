// examples-shared/driver/server/sequence.mjs
//
// Runs a JSON test sequence on every targeted host in parallel and produces a
// per-step comparison, so one invocation answers "what did Android do, what
// did iOS do, what did the Node CLI, Tauri, Electron and Chrome do, where do
// they differ". A sequence-level `devices` map binds each target to its own
// strap, so two hosts can run the same steps concurrently without contending
// for one peripheral.

import { runOnHosts } from './client.mjs'
import { mismatches } from './match.mjs'
import { matchesTarget } from './targets.mjs'

const STEP_KINDS = ['run', 'waitForEvent', 'waitForSnapshot', 'expectSnapshot', 'sleep', 'note']
const DEFAULT_TIMEOUT_MS = 30_000
const SINCE = ['step', 'previous-step', 'sequence']

export function validateSequence(spec) {
  const problems = []
  if (typeof spec !== 'object' || spec === null) throw new Error('sequence must be a JSON object')
  if (typeof spec.name !== 'string') problems.push('sequence.name must be a string')
  if (spec.target !== undefined && !isTarget(spec.target)) problems.push('sequence.target must be a target or a non-empty array of targets')
  if (spec.devices !== undefined) {
    if (typeof spec.devices !== 'object' || spec.devices === null || Array.isArray(spec.devices)) {
      problems.push('sequence.devices must be an object mapping a host id, host kind or platform to a device name')
    } else {
      for (const [key, device] of Object.entries(spec.devices)) {
        if (typeof device !== 'string' || device.trim().length === 0) problems.push(`sequence.devices.${key} must be a non-empty device name`)
      }
    }
  }
  if (!Array.isArray(spec.steps) || spec.steps.length === 0) problems.push('sequence.steps must be a non-empty array')
  for (const [index, step] of (Array.isArray(spec.steps) ? spec.steps : []).entries()) {
    const kinds = STEP_KINDS.filter(kind => step[kind] !== undefined)
    if (kinds.length !== 1) problems.push(`step ${index}: exactly one of ${STEP_KINDS.join(', ')} is required (found ${kinds.join(', ') || 'none'})`)
    if (step.run !== undefined && typeof step.command !== 'string') problems.push(`step ${index}: run requires "command"`)
    if (step.waitForEvent !== undefined && typeof step.kind !== 'string') problems.push(`step ${index}: waitForEvent requires "kind"`)
    if ((step.waitForSnapshot !== undefined || step.expectSnapshot !== undefined) && typeof step.match !== 'object') {
      problems.push(`step ${index}: snapshot steps require "match"`)
    }
    if (step.sleep !== undefined && typeof step.sleep !== 'number') problems.push(`step ${index}: sleep must be milliseconds`)
    if (step.since !== undefined && !SINCE.includes(step.since)) problems.push(`step ${index}: since must be one of ${SINCE.join(', ')}`)
    if (step.platforms !== undefined) problems.push(`step ${index}: "platforms" was renamed to "hosts" in ubm-test-driver/1`)
    if (step.hosts !== undefined && (!Array.isArray(step.hosts) || !step.hosts.every(target => typeof target === 'string'))) {
      problems.push(`step ${index}: hosts must be an array of targets (host id, host kind or platform)`)
    }
  }
  if (problems.length > 0) throw new Error(`invalid sequence:\n  ${problems.join('\n  ')}`)
  return spec
}

function isTarget(target) {
  if (typeof target === 'string') return target.length > 0
  return Array.isArray(target) && target.length > 0 && target.every(entry => typeof entry === 'string' && entry.length > 0)
}

/** The device a sequence binds to one host: host id, then host kind, then platform; null when unbound. */
export function resolveDevice(devices, host) {
  if (devices === undefined || devices === null) return null
  for (const key of [host.hostId, host.host, host.platform]) {
    if (Object.hasOwn(devices, key)) return devices[key]
  }
  return null
}

function commandAcceptsDevice(host, scenario, command) {
  const description = (host.scenarios ?? []).find(entry => entry.id === scenario)
  return description?.commands.find(entry => entry.name === command)?.acceptsDevice === true
}

/** The run step's args for this host: the bound device is added when the command takes one and the step names none. */
function stepArgs(step, host, device) {
  const args = step.args ?? {}
  if (device === null || args.device !== undefined || !commandAcceptsDevice(host, step.run, step.command)) return args
  return { ...args, device }
}

export function describeStep(step, index) {
  if (step.name !== undefined) return `${index}. ${step.name}`
  if (step.run !== undefined) return `${index}. run ${step.run}.${step.command}`
  if (step.waitForEvent !== undefined) return `${index}. wait ${step.waitForEvent}:${step.kind}${step.count > 1 ? ` x${step.count}` : ''}`
  if (step.waitForSnapshot !== undefined) return `${index}. wait snapshot ${step.waitForSnapshot}`
  if (step.expectSnapshot !== undefined) return `${index}. expect snapshot ${step.expectSnapshot}`
  if (step.sleep !== undefined) return `${index}. sleep ${step.sleep}ms`
  return `${index}. note`
}

export async function runSequence(client, spec, { target = spec.target ?? 'all', onProgress = () => {} } = {}) {
  validateSequence(spec)
  const { hosts } = await client.request({ type: 'list' })
  const selected = hosts.filter(host => matchesTarget(host, target))
  if (selected.length === 0) {
    throw new Error(`no connected host matches "${target}"; connected: ${hosts.map(host => host.hostId).join(', ') || 'none'}`)
  }
  if (spec.devices !== undefined) {
    const unbound = selected.filter(host => resolveDevice(spec.devices, host) === null)
    if (unbound.length > 0) {
      throw Object.assign(
        new Error(
          `sequence binds devices per target, but ${unbound.map(host => host.hostId).join(', ')} has no binding in sequence.devices ` +
            `(${Object.keys(spec.devices).join(', ')}); bind it or narrow --target so no host runs on an unchosen strap`
        ),
        { code: 'sequence.device-unbound' }
      )
    }
  }
  const observers = new Map(selected.map(host => [host.hostId, createObserver()]))
  const { hosts: cached } = await client.request({ type: 'snapshots', target })
  for (const entry of cached) {
    const observer = observers.get(entry.hostId)
    for (const [scenario, value] of Object.entries(entry.snapshots)) observer?.snapshots.set(scenario, value.snapshot)
  }
  const stopListening = client.onRecord(record => {
    const observer = observers.get(record.hostId)
    if (observer !== undefined) observer.accept(record)
  })
  const startedAt = new Date().toISOString()
  try {
    const results = await Promise.all(
      selected.map(host => runHost(client, spec, host, resolveDevice(spec.devices, host), observers.get(host.hostId), onProgress))
    )
    return { sequence: spec.name, target, startedAt, finishedAt: new Date().toISOString(), hosts: results }
  } finally {
    stopListening()
  }
}

function createObserver() {
  const waiters = new Set()
  const observer = {
    snapshots: new Map(),
    events: [],
    connected: true,
    accept(record) {
      const message = record.message
      if (record.event === 'host-disconnected') observer.connected = false
      if (record.event === 'host-connected') observer.connected = true
      if (message?.type === 'snapshot') observer.snapshots.set(message.scenario, message.snapshot)
      if (message?.type === 'event') observer.events.push(message.event)
      for (const waiter of waiters) waiter()
    },
    waitUntil(predicate, timeoutMs) {
      return new Promise(resolve => {
        const check = () => {
          const value = predicate()
          if (value === undefined) return
          finish({ ok: true, value })
        }
        const finish = outcome => {
          clearTimeout(timer)
          waiters.delete(check)
          resolve(outcome)
        }
        const timer = setTimeout(() => finish({ ok: false }), timeoutMs)
        waiters.add(check)
        check()
      })
    }
  }
  return observer
}

async function runHost(client, spec, host, device, observer, onProgress) {
  const steps = []
  let failed = false
  const stepStarts = []
  for (const [index, step] of spec.steps.entries()) {
    const label = describeStep(step, index)
    stepStarts[index] = observer.events.length
    if (step.hosts !== undefined && !step.hosts.some(target => matchesTarget(host, target))) {
      steps.push({ index, label, status: 'skipped', detail: { reason: `hosts ${step.hosts.join(',')}` } })
      continue
    }
    if (failed && step.always !== true) {
      steps.push({ index, label, status: 'skipped', detail: { reason: 'earlier step failed' } })
      continue
    }
    onProgress({ type: 'step-start', hostId: host.hostId, host: host.host, platform: host.platform, device, index, label })
    const started = Date.now()
    let outcome
    try {
      outcome = await executeStep(client, spec, step, host, device, observer, index === 0 ? 0 : stepStarts[sinceIndex(step, index)])
    } catch (error) {
      outcome = { passed: false, detail: { error: { code: error.code ?? error.name, message: error.message } } }
    }
    const captured = captureKeys(step, observer)
    const entry = {
      index,
      label,
      status: outcome.passed ? 'passed' : 'failed',
      durationMs: Date.now() - started,
      detail: captured === null ? outcome.detail : { ...outcome.detail, captured }
    }
    steps.push(entry)
    onProgress({ type: 'step-end', hostId: host.hostId, host: host.host, platform: host.platform, ...entry })
    if (!outcome.passed) failed = true
  }
  return { hostId: host.hostId, host: host.host, platform: host.platform, backend: host.backend, model: host.model, device, passed: !failed, steps }
}

function sinceIndex(step, index) {
  const since = step.since ?? 'previous-step'
  if (since === 'sequence') return 0
  if (since === 'step') return index
  return Math.max(0, index - 1)
}

function captureKeys(step, observer) {
  if (!Array.isArray(step.capture)) return null
  const scenario = step.run ?? step.waitForEvent ?? step.waitForSnapshot ?? step.expectSnapshot
  const snapshot = observer.snapshots.get(scenario) ?? {}
  return Object.fromEntries(step.capture.map(key => [key, snapshot[key] ?? null]))
}

async function executeStep(client, spec, step, host, device, observer, eventStart) {
  const timeoutMs = step.timeoutMs ?? spec.defaultTimeoutMs ?? DEFAULT_TIMEOUT_MS
  if (step.note !== undefined) return { passed: true, detail: { note: step.note } }
  if (step.sleep !== undefined) {
    await new Promise(resolve => setTimeout(resolve, step.sleep))
    return { passed: true, detail: {} }
  }
  if (step.run !== undefined) {
    const args = stepArgs(step, host, device)
    const [answer] = await runOnHosts(client, { target: host.hostId, scenario: step.run, command: step.command, args, timeoutMs })
    const observed = answer.ok ? { ok: true, result: answer.result } : { ok: false, error: answer.error }
    const problems = mismatches(observed, step.expect ?? { ok: true })
    return { passed: problems.length === 0, detail: problems.length === 0 ? observed : { ...observed, mismatches: problems } }
  }
  if (step.waitForEvent !== undefined) {
    const count = step.count ?? 1
    const matching = () =>
      observer.events
        .slice(eventStart)
        .filter(event => event.scenario === step.waitForEvent && event.kind === step.kind && mismatches(event.data, step.where ?? {}).length === 0)
    const outcome = await observer.waitUntil(() => {
      const found = matching()
      return found.length >= count ? found : undefined
    }, timeoutMs)
    const found = outcome.ok ? outcome.value : matching()
    return {
      passed: outcome.ok,
      detail: outcome.ok
        ? { matched: found.length, last: found.at(-1)?.data ?? null }
        : { error: { code: 'sequence.wait-timeout', message: `saw ${found.length}/${count} ${step.kind} event(s) within ${timeoutMs} ms` } }
    }
  }
  const scenario = step.waitForSnapshot ?? step.expectSnapshot
  const check = () => {
    const snapshot = observer.snapshots.get(scenario)
    return snapshot !== undefined && mismatches(snapshot, step.match).length === 0 ? snapshot : undefined
  }
  if (step.expectSnapshot !== undefined) {
    const snapshot = observer.snapshots.get(scenario)
    const problems = snapshot === undefined ? [`no snapshot received for ${scenario}`] : mismatches(snapshot, step.match)
    return { passed: problems.length === 0, detail: problems.length === 0 ? {} : { mismatches: problems } }
  }
  const outcome = await observer.waitUntil(check, timeoutMs)
  if (outcome.ok) return { passed: true, detail: {} }
  const snapshot = observer.snapshots.get(scenario)
  return {
    passed: false,
    detail: {
      error: { code: 'sequence.wait-timeout', message: `snapshot did not match within ${timeoutMs} ms` },
      mismatches: snapshot === undefined ? [`no snapshot received for ${scenario}`] : mismatches(snapshot, step.match)
    }
  }
}

/** The peer a run step reported acquiring (every peer-acquiring command returns `peer`), or null. */
function reportedDevice(detail) {
  const peer = detail?.result?.peer
  if (typeof peer !== 'object' || peer === null) return null
  return peer.name ?? peer.id ?? null
}

/**
 * One row per step, one cell per host; `differs` marks steps whose status is
 * not the same everywhere. A cell's `device` is the peer that step reported.
 */
export function compareSummary(summary) {
  const rows = []
  const stepCount = Math.max(...summary.hosts.map(host => host.steps.length))
  for (let index = 0; index < stepCount; index += 1) {
    const cells = Object.fromEntries(
      summary.hosts.map(host => {
        const step = host.steps[index]
        return [
          host.hostId,
          step === undefined ? null : { status: step.status, durationMs: step.durationMs ?? null, device: reportedDevice(step.detail), detail: step.detail }
        ]
      })
    )
    const statuses = new Set(Object.values(cells).map(cell => cell?.status ?? 'missing'))
    rows.push({ index, label: summary.hosts[0].steps[index]?.label ?? String(index), differs: statuses.size > 1, cells })
  }
  return rows
}

function cellText(cell) {
  if (cell === null) return 'missing'
  const code = cell.detail?.error?.code
  const duration = cell.durationMs === null ? '' : ` ${cell.durationMs}ms`
  return `${cell.status}${duration}${code === undefined ? '' : ` ${code}`}${cell.device === null ? '' : ` @ ${cell.device}`}`
}

/** Side-by-side table: a device row (the sequence binding per host), then per step each host's outcome and reported device. */
export function formatComparison(summary) {
  const rows = compareSummary(summary)
  const hostIds = summary.hosts.map(host => host.hostId)
  const labelWidth = Math.max(10, ...rows.map(row => row.label.length + 1))
  const deviceRow = summary.hosts.map(host => host.device ?? '(default)')
  const cellRows = rows.map(row => hostIds.map(id => cellText(row.cells[id])))
  const widths = hostIds.map((id, column) => Math.max(24, id.length, deviceRow[column].length, ...cellRows.map(cells => cells[column].length)))
  const line = (label, cells) => [label.padEnd(labelWidth).slice(0, labelWidth), ...cells.map((cell, column) => cell.padEnd(widths[column]))].join(' | ')
  const lines = [line('', hostIds), line(' device', deviceRow)]
  rows.forEach((row, index) => lines.push(line(`${row.differs ? '*' : ' '}${row.label}`, cellRows[index])))
  lines.push(
    ...summary.hosts.map(
      host => `${host.hostId} (${host.host}/${host.platform}, ${host.backend}${host.device === null ? '' : `, device ${host.device}`}): ${host.passed ? 'PASSED' : 'FAILED'}`
    )
  )
  return lines.join('\n')
}
