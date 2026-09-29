#!/usr/bin/env node
// example-node/driver.ts
//
// Node desktop host of the shared test driver. Run with Node >= 22.18 (type
// stripping) from the repository root after `pnpm prepack`:
//
//   node example-node/driver.ts serve-host [--backend corebluetooth|winrt|bluez] [--driver-url ws://…/host|off]
//   node example-node/driver.ts run <scenario> <command> [json-args] [--backend …] [--for MS]
//   node example-node/driver.ts list
//
// `serve-host` connects to the control server and lets it drive this
// process exactly like the phones; `run` dispatches one command locally and
// prints every event as a JSON line.

import {
  LOCAL_DRIVER_URL,
  createRemoteDriver,
  createScenarioRegistry,
  describeError,
  explicitDriverUrl,
  isJsonObject,
  type DriverUrlResolution,
  type JsonObject
} from '../examples-shared/driver/index.ts'
import { createNodeDriverHost, nodeSocket, parseBackend } from './host.ts'
import { shutdownNodeDriver, createNodeShutdownHandler } from './cleanup.ts'
import { flushDriverOutput } from './output.ts'

async function exitAfterOutput(code: number): Promise<never> {
  let exitCode = code
  try {
    await flushDriverOutput(process.stdout, process.stderr)
  } catch (error) {
    exitCode = 1
    process.stderr.write(
      `[example-node] output completion failed: ${error instanceof Error ? error.message : String(error)}\n`
    )
    try {
      await flushDriverOutput(process.stderr, process.stderr)
    } catch (diagnosticError) {
      process.stderr.write(
        `[example-node] diagnostic output also failed: ${diagnosticError instanceof Error ? diagnosticError.message : String(diagnosticError)}\n`
      )
    }
  }
  process.exit(exitCode)
}

const HELP = `example-node/driver.ts — the shared BLE test scenarios on a Node desktop host

  serve-host [--backend B] [--adapter ID] [--driver-url URL|off]   Connect to the control server (default ${LOCAL_DRIVER_URL}) until Ctrl-C.
  run <scenario> <command> [json-args] [--backend B] [--adapter ID] [--for MS]
  --bluez-daemon-owner NAME   Trusted BlueZ LE1 daemon attestation (or UBM_BLUEZ_DAEMON_OWNER); BlueZ only.
                                                  Dispatch locally, print events as JSON lines; with --for, keep
                                                  the run going MS milliseconds, then dispatch "stop".
  list                                            Scenario descriptions (JSON).

  B is corebluetooth (macOS default) | winrt (Windows default) | bluez (Linux default).
  UBM_DRIVER_URL may replace --driver-url.`

type Flags = Readonly<Record<string, string | true>>

function parseArgs(argv: readonly string[]): { positional: string[]; flags: Flags } {
  const positional: string[] = []
  const flags: Record<string, string | true> = {}
  for (let index = 0; index < argv.length; index += 1) {
    const value = argv[index] ?? ''
    if (!value.startsWith('--')) {
      positional.push(value)
      continue
    }
    if (!['backend', 'adapter', 'driver-url', 'for', 'bluez-daemon-owner'].includes(value.slice(2)))
      fail(`unknown option ${value}`)
    const next = argv[index + 1]
    if (next === undefined || next.startsWith('--')) flags[value.slice(2)] = true
    else {
      flags[value.slice(2)] = next
      index += 1
    }
  }
  return { positional, flags }
}

function stringFlag(flags: Flags, name: string): string | undefined {
  const value = flags[name]
  if (value === true) fail(`--${name} needs a value`)
  return value
}

function fail(message: string, code = 2): never {
  process.stderr.write(`${message}\n`)
  process.exit(code)
}

function printLine(value: unknown): void {
  process.stdout.write(`${JSON.stringify(value)}\n`)
}

function driverUrl(flags: Flags): DriverUrlResolution {
  return (
    explicitDriverUrl(stringFlag(flags, 'driver-url'), '--driver-url') ??
    explicitDriverUrl(process.env.UBM_DRIVER_URL, 'UBM_DRIVER_URL') ?? {
      url: LOCAL_DRIVER_URL,
      reason: 'default local control server'
    }
  )
}

async function serveHost(flags: Flags): Promise<void> {
  const host = createNodeDriverHost(
    parseBackend(stringFlag(flags, 'backend'), process.platform),
    stringFlag(flags, 'adapter'),
    { bluezDaemonOwner: stringFlag(flags, 'bluez-daemon-owner') ?? process.env.UBM_BLUEZ_DAEMON_OWNER }
  )
  const registry = createScenarioRegistry(host)
  const resolution = driverUrl(flags)
  if (resolution.url === null) fail(`no control server: ${resolution.reason}`)
  const remote = createRemoteDriver(host, registry, { ...resolution, createSocket: nodeSocket })
  remote.subscribe(state =>
    process.stderr.write(
      `[example-node] remote ${state.status}${state.hostId === null ? '' : ` as ${state.hostId}`}${state.lastError === null ? '' : ` (${state.lastError})`}\n`
    )
  )
  process.stderr.write(`[example-node] ${host.identity.backend} → ${resolution.url} (${resolution.reason})\n`)
  remote.start()
  const shutdown = createNodeShutdownHandler({
    cleanup: async () => {
      remote.stop()
      await shutdownNodeDriver(registry, host, printLine)
    },
    released: () => exitAfterOutput(0),
    failed: async error => {
      printLine({ type: 'shutdown-failed', error: describeError(error) })
      process.stderr.write('[example-node] cleanup remains owned; send SIGINT or SIGTERM again to retry\n')
      await flushDriverOutput(process.stdout, process.stderr).catch(outputError => {
        process.stderr.write(`[example-node] shutdown diagnostic flush failed: ${String(outputError)}\n`)
      })
    }
  })
  process.on('SIGINT', shutdown)
  process.on('SIGTERM', shutdown)
}

function parseJsonArgs(raw: string | undefined): JsonObject {
  if (raw === undefined) return {}
  let parsed: unknown
  try {
    parsed = JSON.parse(raw)
  } catch (error) {
    fail(`json-args is not valid JSON: ${error instanceof Error ? error.message : String(error)}`)
  }
  if (!isJsonObject(parsed)) fail('json-args must be a JSON object')
  return parsed
}

async function runLocal(positional: readonly string[], flags: Flags): Promise<void> {
  const [scenarioId, command, rawArgs] = positional
  if (scenarioId === undefined || command === undefined) fail('usage: run <scenario> <command> [json-args]')
  const forMs = Number(stringFlag(flags, 'for') ?? '0')
  if (!Number.isFinite(forMs) || forMs < 0) fail('--for must be a non-negative number of milliseconds')
  const host = createNodeDriverHost(
    parseBackend(stringFlag(flags, 'backend'), process.platform),
    stringFlag(flags, 'adapter'),
    { bluezDaemonOwner: stringFlag(flags, 'bluez-daemon-owner') ?? process.env.UBM_BLUEZ_DAEMON_OWNER }
  )
  const registry = createScenarioRegistry(host)
  registry.subscribe(update => printLine(update))
  let failed = false
  try {
    printLine({
      type: 'result',
      scenario: scenarioId,
      command,
      result: await registry.dispatch(scenarioId, command, parseJsonArgs(rawArgs))
    })
    if (forMs > 0) await new Promise(resolve => setTimeout(resolve, forMs))
  } catch (error) {
    failed = true
    printLine({ type: 'error', scenario: scenarioId, command, error: describeError(error) })
  }
  try {
    await shutdownNodeDriver(registry, host, printLine)
  } catch (error) {
    failed = true
    printLine({ type: 'shutdown-failed', error: describeError(error) })
  }
  await exitAfterOutput(failed ? 1 : 0)
}

async function main(): Promise<void> {
  const { positional, flags } = parseArgs(process.argv.slice(2))
  const [command, ...rest] = positional
  switch (command) {
    case 'serve-host':
      await serveHost(flags)
      return
    case 'run':
      await runLocal(rest, flags)
      return
    case 'list': {
      const host = createNodeDriverHost(
        parseBackend(stringFlag(flags, 'backend'), process.platform),
        stringFlag(flags, 'adapter'),
        { bluezDaemonOwner: stringFlag(flags, 'bluez-daemon-owner') ?? process.env.UBM_BLUEZ_DAEMON_OWNER }
      )
      printLine(createScenarioRegistry(host).describe())
      return
    }
    case undefined:
    case 'help':
      process.stdout.write(`${HELP}\n`)
      return
    default:
      fail(`unknown command "${command}"\n\n${HELP}`)
  }
}

await main()
