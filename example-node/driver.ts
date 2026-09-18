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
  type JsonObject,
  type ScenarioRegistry
} from '../examples-shared/driver/index.ts'
import { createNodeDriverHost, nodeSocket, parseBackend } from './host.ts'

const HELP = `example-node/driver.ts — the shared BLE test scenarios on a Node desktop host

  serve-host [--backend B] [--driver-url URL|off]   Connect to the control server (default ${LOCAL_DRIVER_URL}) until Ctrl-C.
  run <scenario> <command> [json-args] [--backend B] [--for MS]
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
    explicitDriverUrl(process.env.UBM_DRIVER_URL, 'UBM_DRIVER_URL') ?? { url: LOCAL_DRIVER_URL, reason: 'default local control server' }
  )
}

/** Stops every scenario (each reports its cleanup records) so no radio resource outlives the process. */
async function stopAll(registry: ScenarioRegistry): Promise<void> {
  for (const scenario of registry.list()) {
    try {
      const result = await scenario.dispatch('stop', {})
      printLine({ type: 'shutdown-stop', scenario: scenario.id, result })
    } catch (error) {
      printLine({ type: 'shutdown-stop', scenario: scenario.id, error: describeError(error) })
    }
  }
}

async function serveHost(flags: Flags): Promise<void> {
  const host = createNodeDriverHost(parseBackend(stringFlag(flags, 'backend'), process.platform))
  const registry = createScenarioRegistry(host)
  const resolution = driverUrl(flags)
  if (resolution.url === null) fail(`no control server: ${resolution.reason}`)
  const remote = createRemoteDriver(host, registry, { ...resolution, createSocket: nodeSocket })
  remote.subscribe(state => process.stderr.write(`[example-node] remote ${state.status}${state.hostId === null ? '' : ` as ${state.hostId}`}${state.lastError === null ? '' : ` (${state.lastError})`}\n`))
  process.stderr.write(`[example-node] ${host.identity.backend} → ${resolution.url} (${resolution.reason})\n`)
  remote.start()
  const shutdown = async () => {
    remote.stop()
    await stopAll(registry)
    process.exit(0)
  }
  process.once('SIGINT', shutdown)
  process.once('SIGTERM', shutdown)
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
  const host = createNodeDriverHost(parseBackend(stringFlag(flags, 'backend'), process.platform))
  const registry = createScenarioRegistry(host)
  registry.subscribe(update => printLine(update))
  let failed = false
  try {
    printLine({ type: 'result', scenario: scenarioId, command, result: await registry.dispatch(scenarioId, command, parseJsonArgs(rawArgs)) })
    if (forMs > 0) await new Promise(resolve => setTimeout(resolve, forMs))
  } catch (error) {
    failed = true
    printLine({ type: 'error', scenario: scenarioId, command, error: describeError(error) })
  }
  await stopAll(registry)
  process.exit(failed ? 1 : 0)
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
      const host = createNodeDriverHost(parseBackend(stringFlag(flags, 'backend'), process.platform))
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
