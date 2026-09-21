// examples-shared/dev/metro-port-guard.js
//
// Finding 241. A React Native example that reaches a dev server belonging to
// another project loads that project's bundle, registers no callable
// JavaScript modules, and then throws on every native call — thousands per
// second, without bound, until someone notices the machine is out of memory.
// Nothing in that sequence says "wrong dev server", so this guard says it
// first, before the app starts.
//
// Ownership is read from the listening process's working directory, which is
// the fact that actually distinguishes "our Metro" from "someone else's". No
// lock files, no state: a guard that can be fooled by a stale file is worse
// than no guard. Where the platform gives us no way to read it, the guard
// says so out loud and steps aside rather than guessing either way.
//
// Usage: node examples-shared/dev/metro-port-guard.js <port> [projectRoot]

'use strict'

const path = require('path')
const { spawnSync } = require('child_process')

/**
 * One exit code per outcome, so a shell caller can tell a free port from an
 * unreadable one. Outcomes that let the run continue are 0.
 */
const EXIT_CODES = Object.freeze({
  free: 0,
  'held-by-project': 0,
  undetermined: 0,
  'held-by-other': 3,
  'held-by-unknown': 4
})

function runProbe(command, args) {
  const result = spawnSync(command, args, { encoding: 'utf8' })
  if (result.error !== undefined || result.status !== 0) return null
  return typeof result.stdout === 'string' ? result.stdout : null
}

function listenerAvailable() {
  const result = spawnSync('lsof', ['-v'], { encoding: 'utf8' })
  return result.error === undefined
}

function findListener(port) {
  const output = runProbe('lsof', ['-nP', `-iTCP:${String(port)}`, '-sTCP:LISTEN', '-Fp'])
  if (output === null) return null
  for (const line of output.split('\n')) {
    if (line.startsWith('p')) {
      const pid = Number.parseInt(line.slice(1), 10)
      if (Number.isInteger(pid)) return pid
    }
  }
  return null
}

function processCwd(pid) {
  const output = runProbe('lsof', ['-a', '-p', String(pid), '-d', 'cwd', '-Fn'])
  if (output === null) return null
  for (const line of output.split('\n')) {
    if (line.startsWith('n')) return line.slice(1)
  }
  return null
}

function processCommand(pid) {
  const output = runProbe('ps', ['-o', 'args=', '-p', String(pid)])
  if (output === null) return null
  const trimmed = output.trim()
  return trimmed === '' ? null : trimmed
}

function isInside(directory, root) {
  const relative = path.relative(root, directory)
  return relative === '' || (!relative.startsWith('..') && !path.isAbsolute(relative))
}

/**
 * Reports who holds `port`, relative to `projectRoot`. The four probes are
 * injected so the decision is testable without a real listening socket.
 */
function inspectPort(options) {
  const { port, projectRoot } = options
  if (!options.listenerAvailable()) {
    return Object.freeze({ state: 'undetermined', ok: true, warning: true, port })
  }

  const pid = options.findListener(port)
  if (pid === null) {
    return Object.freeze({ state: 'free', ok: true, warning: false, port })
  }

  const cwd = options.processCwd(pid)
  const command = options.processCommand(pid)
  if (cwd === null) {
    return Object.freeze({ state: 'held-by-unknown', ok: false, warning: false, port, pid, command })
  }
  if (isInside(cwd, projectRoot)) {
    return Object.freeze({ state: 'held-by-project', ok: true, warning: false, port, pid, cwd, command })
  }
  return Object.freeze({ state: 'held-by-other', ok: false, warning: false, port, pid, cwd, command })
}

function describeOutcome(outcome) {
  const port = String(outcome.port)
  switch (outcome.state) {
    case 'free':
      return `metro-port-guard: port ${port} is free.`
    case 'held-by-project':
      return `metro-port-guard: port ${port} is held by this project (pid ${String(outcome.pid)}).`
    case 'undetermined':
      return `metro-port-guard: cannot determine who holds port ${port} on this platform (no lsof). Continuing unverified: if the app loads a bundle from another project you will see "Module has not been registered as callable".`
    case 'held-by-other':
      return [
        `metro-port-guard: port ${port} is held by another project, not this example.`,
        `  pid ${String(outcome.pid)} running in ${String(outcome.cwd)}`,
        `  ${outcome.command === null || outcome.command === undefined ? '(command unavailable)' : outcome.command}`,
        '',
        "Starting here would load that project's bundle: no JavaScript module would register,",
        'and React Native would throw on every native call, without bound, until the machine',
        'runs out of memory. Free the port, or point this example at another one',
        '(RCT_METRO_PORT for the bare example).'
      ].join('\n')
    case 'held-by-unknown':
      return [
        `metro-port-guard: port ${port} is held by pid ${String(outcome.pid)}, but the guard could not read its working directory, so it cannot tell whether it belongs to this project.`,
        `  ${outcome.command === null || outcome.command === undefined ? '(command unavailable)' : outcome.command}`,
        'Check it yourself, or free the port.'
      ].join('\n')
    default:
      throw new Error(`metro-port-guard: unhandled outcome ${String(outcome.state)}`)
  }
}

function main(argv) {
  const port = Number.parseInt(argv[0] ?? '', 10)
  if (!Number.isInteger(port) || port <= 0 || port > 65535) {
    throw new Error('Usage: node examples-shared/dev/metro-port-guard.js <port> [projectRoot]')
  }
  const projectRoot = path.resolve(argv[1] ?? process.cwd())
  const outcome = inspectPort({
    port,
    projectRoot,
    listenerAvailable,
    findListener,
    processCwd,
    processCommand
  })
  const message = describeOutcome(outcome)
  if (outcome.ok && !outcome.warning) {
    console.log(message)
  } else {
    console.error(message)
  }
  process.exitCode = EXIT_CODES[outcome.state]
}

if (require.main === module) main(process.argv.slice(2))

module.exports = { inspectPort, describeOutcome, EXIT_CODES }
