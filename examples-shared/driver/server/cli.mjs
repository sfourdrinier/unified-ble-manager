#!/usr/bin/env node
// examples-shared/driver/server/cli.mjs
//
// Control server and CLI of the cross-host test driver (protocol
// ubm-test-driver/1). See `node examples-shared/driver/server/cli.mjs help`
// and examples-shared/driver/README.md.

import { appendFileSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'

const [major, minor] = process.versions.node.split('.').map(Number)
if (major < 22 || (major === 22 && minor < 18)) {
  process.stderr.write(`ubm-driver needs Node >= 22.18 (built-in WebSocket client and TypeScript stripping); this is ${process.version}\n`)
  process.exit(2)
}

// The shared wire contract is ../protocol.ts, loaded through Node's type
// stripping. It sits in the repository's root package scope, which declares
// no "type", so Node notes that it re-parsed the .ts file as ESM; that note is
// expected and is the only warning filtered here.
process.removeAllListeners('warning')
process.on('warning', warning => {
  if (warning.code !== 'MODULE_TYPELESS_PACKAGE_JSON') process.stderr.write(`${warning.name}: ${warning.message}\n`)
})
const { createHub, DEFAULT_DRIVER_PORT } = await import('./hub.mjs')
const { connectControl, runOnHosts } = await import('./client.mjs')
const { matchesTarget } = await import('./targets.mjs')
const { runSequence, formatComparison, validateSequence } = await import('./sequence.mjs')

const HELP = `ubm-driver — run the shared test scenarios on every connected host (ubm-test-driver/1)

  serve [--port ${DEFAULT_DRIVER_PORT}] [--host 0.0.0.0] [--log-dir DIR] [--quiet]
        Run the control server. Hosts connect to ws://<this machine>:${DEFAULT_DRIVER_PORT}/host.
        Every message is written as a JSON line to stdout (unless --quiet) and to DIR/driver-<time>.jsonl.
  hosts                                  List connected hosts (JSON).
  describe <target>                      Scenarios and commands the host(s) expose.
  run <target> <scenario> <command> [json-args] [--timeout MS] [--follow MS]
        Dispatch through each host's scenario registry; stream the targeted hosts' messages as JSON
        lines until every host answered (plus --follow MS). Exit 1 if any host answered an error.
  watch [target] [--scenario ID]         Stream host messages as JSON lines until interrupted.
  snapshot <target> [scenario]           Latest snapshot(s) the server holds.
  sequence <file.json> [--target T[,T…]] [--out summary.json]
        Run a scripted test on every targeted host in parallel; prints progress and a final
        side-by-side comparison (per step, per host: outcome and the device the step reported).
        A sequence "devices" map binds each target to its own strap. Exit 1 if any host failed a step.

  <target> is "all", a host id from \`hosts\`, a host kind (expo | web | tauri | electron | node)
  or a platform (android | ios | macos | windows | linux).
  Every peer-acquiring command takes {"device": "<exact advertised name>"} or {"device": "<prefix>*"},
  e.g. run android h10-stream start '{"device":"Polar H10 E997042F"}'.
  --server ws://HOST:PORT/control selects the server for client commands (default ws://127.0.0.1:${DEFAULT_DRIVER_PORT}/control).`

function parseArgs(argv) {
  const positional = []
  const flags = {}
  for (let index = 0; index < argv.length; index += 1) {
    const value = argv[index]
    if (value.startsWith('--')) {
      const name = value.slice(2)
      const next = argv[index + 1]
      if (next === undefined || next.startsWith('--')) flags[name] = true
      else {
        flags[name] = next
        index += 1
      }
    } else positional.push(value)
  }
  return { positional, flags }
}

function printLine(value) {
  process.stdout.write(`${JSON.stringify(value)}\n`)
}

function fail(message, code = 2) {
  process.stderr.write(`${message}\n`)
  process.exit(code)
}

function numberFlag(flags, name, fallback) {
  if (flags[name] === undefined) return fallback
  const value = Number(flags[name])
  if (!Number.isFinite(value) || value < 0) fail(`--${name} must be a non-negative number of milliseconds`)
  return value
}

/** `--target android,ios` selects several targets; a single target stays a string. */
function parseTargetList(value) {
  const targets = value.split(',').map(entry => entry.trim())
  if (targets.some(entry => entry.length === 0)) fail(`--target has an empty entry: "${value}"`)
  return targets.length === 1 ? targets[0] : targets
}

function recordConcernsTarget(record, target) {
  return matchesTarget(record, target)
}

async function serve(flags) {
  const port = numberFlag(flags, 'port', DEFAULT_DRIVER_PORT)
  const logDir = resolve(typeof flags['log-dir'] === 'string' ? flags['log-dir'] : join(tmpdir(), 'ubm-test-driver'))
  mkdirSync(logDir, { recursive: true })
  const logFile = join(logDir, `driver-${new Date().toISOString().replace(/[:.]/g, '-')}.jsonl`)
  const quiet = flags.quiet === true
  const hub = createHub({
    port,
    host: typeof flags.host === 'string' ? flags.host : '0.0.0.0',
    onRecord: record => {
      const line = JSON.stringify(record)
      appendFileSync(logFile, `${line}\n`)
      if (!quiet) process.stdout.write(`${line}\n`)
    }
  })
  let address
  try {
    address = await hub.listen()
  } catch (error) {
    fail(`cannot listen on ${port}: ${error.message}${error.code === 'EADDRINUSE' ? ' (another driver running? check: lsof -iTCP:' + port + ' -sTCP:LISTEN)' : ''}`)
  }
  process.stderr.write(
    `[ubm-driver] listening on ${address.host}:${address.port}  hosts: /host  control: /control  log: ${logFile}\n` +
      `[ubm-driver] Android over USB (Metro via localhost): adb reverse tcp:${address.port} tcp:${address.port}\n`
  )
  const shutdown = async () => {
    await hub.close()
    process.exit(0)
  }
  process.on('SIGINT', shutdown)
  process.on('SIGTERM', shutdown)
}

async function withClient(flags, action) {
  const client = await connectControl(typeof flags.server === 'string' ? flags.server : undefined).catch(error => fail(error.message))
  try {
    return await action(client)
  } finally {
    await client.close()
  }
}

async function main() {
  const { positional, flags } = parseArgs(process.argv.slice(2))
  const [command, ...rest] = positional
  switch (command) {
    case 'serve':
      await serve(flags)
      return
    case 'hosts':
      await withClient(flags, async client => printLine((await client.request({ type: 'list' })).hosts))
      return
    case 'describe': {
      const target = rest[0] ?? 'all'
      await withClient(flags, async client => {
        const { hosts } = await client.request({ type: 'list' })
        for (const host of hosts.filter(entry => matchesTarget(entry, target))) {
          printLine({ hostId: host.hostId, host: host.host, platform: host.platform, backend: host.backend, scenarios: host.scenarios })
        }
      })
      return
    }
    case 'snapshot': {
      const [target = 'all', scenario] = rest
      await withClient(flags, async client => {
        const { hosts } = await client.request({ type: 'snapshots', target })
        for (const host of hosts) {
          printLine({ hostId: host.hostId, host: host.host, platform: host.platform, snapshots: scenario === undefined ? host.snapshots : { [scenario]: host.snapshots[scenario] ?? null } })
        }
      })
      return
    }
    case 'watch': {
      const target = rest[0]
      await withClient(flags, async client => {
        client.onRecord(record => {
          if (!recordConcernsTarget(record, target)) return
          if (typeof flags.scenario === 'string') {
            const message = record.message
            const scenario = message?.scenario ?? message?.event?.scenario
            if (scenario !== undefined && scenario !== flags.scenario) return
          }
          printLine(record)
        })
        await new Promise(() => {})
      })
      return
    }
    case 'run': {
      const [target, scenario, scenarioCommand, rawArgs] = rest
      if (target === undefined || scenario === undefined || scenarioCommand === undefined) fail('usage: run <target> <scenario> <command> [json-args]')
      let args = {}
      if (rawArgs !== undefined) {
        try {
          args = JSON.parse(rawArgs)
        } catch (error) {
          fail(`json-args is not valid JSON: ${error.message}`)
        }
      }
      const timeoutMs = numberFlag(flags, 'timeout', 120_000)
      const followMs = numberFlag(flags, 'follow', 0)
      const failed = await withClient(flags, async client => {
        client.onRecord(record => {
          if (recordConcernsTarget(record, target)) printLine(record)
        })
        const outcomes = await runOnHosts(client, { target, scenario, command: scenarioCommand, args, timeoutMs })
        if (followMs > 0) await new Promise(resolve => setTimeout(resolve, followMs))
        printLine({ type: 'run-summary', scenario, command: scenarioCommand, args, outcomes })
        return outcomes.some(outcome => !outcome.ok)
      }).catch(error => fail(`${error.code ?? 'error'}: ${error.message}`, 1))
      process.exit(failed ? 1 : 0)
      return
    }
    case 'sequence': {
      const [file] = rest
      if (file === undefined) fail('usage: sequence <file.json> [--target T] [--out summary.json]')
      let spec
      try {
        spec = validateSequence(JSON.parse(readFileSync(file, 'utf8')))
      } catch (error) {
        fail(`${file}: ${error.message}`)
      }
      const target = typeof flags.target === 'string' ? parseTargetList(flags.target) : undefined
      const summary = await withClient(flags, async client => {
        client.onRecord(record => {
          if (recordConcernsTarget(record, target ?? spec.target)) printLine(record)
        })
        return runSequence(client, spec, { target, onProgress: printLine })
      }).catch(error => fail(`${error.code ?? 'error'}: ${error.message}`, 1))
      printLine({ type: 'sequence-summary', ...summary })
      if (typeof flags.out === 'string') writeFileSync(flags.out, `${JSON.stringify(summary, null, 2)}\n`)
      process.stderr.write(`\n${formatComparison(summary)}\n`)
      process.exit(summary.hosts.every(host => host.passed) ? 0 : 1)
      return
    }
    case undefined:
    case 'help':
    case '--help':
      process.stdout.write(`${HELP}\n`)
      return
    default:
      fail(`unknown command "${command}"\n\n${HELP}`)
  }
}

await main()
