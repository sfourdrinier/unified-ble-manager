// src/tck/rust-driver/rust-driver.ts
//
// U7 parity slice: thin TypeScript driver over the REAL napi `.node` build
// of the transition-driving core (trackourhealth/bun-mono#1188, U7).
//
// This module is mechanical only: it loads no addon itself (the caller
// passes the loaded native module, so tests fail loudly when the real
// `.node` build is absent instead of falling back), validates nothing, and
// owns no contract constants. Contract truth stays single-owned by
// `ubm-core` (Rust) and the frozen `contracts/**` text; observations here
// are compared against those sources by the parity tests, never adjusted
// to match.

/**
 * Structural surface of one native session. Mirrors the napi `EchoSession`
 * exports used by the parity slice (strings only, so no `Buffer` typing is
 * needed here). Every failure throws a JS `Error` whose message carries the
 * frozen `code|domain|operation|detail` wire form.
 */
export interface RustNativeSession {
  centralStatus(): string
  driveExpireSweep(nowMsDecimal: string): string
  driveDestroy(): string
  requestBleTransition(transition: string): void
  echoCounter(decimal: string): string
  stagedStep(line: string): string
  stagedDrainLog(): string
  stagedCounters(): string
  close(): void
}

/** Structural surface of the loaded napi addon used by the parity slice. */
export interface RustNativeAddon {
  echoRevision(): string
  echoMaxBytes(): number
  EchoSession: new (revision: string) => RustNativeSession
}

/** Parsed `code|domain|operation|detail` contract identity. */
export interface RustWireError {
  readonly code: string
  readonly domain: string
  readonly operation: string
  readonly detail: string
}

/**
 * Reads the message of a rejection across VM realms (jest runs test files
 * in a sandbox whose `Error` differs from the realm that constructed a
 * native rejection, so `instanceof Error` is unreliable here by design).
 */
function messageOf(error: unknown): string {
  if (typeof error === 'string') {
    return error
  }
  if (typeof error === 'object' && error !== null && 'message' in error && typeof error.message === 'string') {
    return error.message
  }
  return String(error)
}

/**
 * Parses a native rejection into its contract identity. Rejections without
 * the wire form fail loudly: an unidentified failure is never recorded as
 * a contract observation.
 */
export function parseRustWireError(error: unknown): RustWireError {
  const message = messageOf(error)
  const parts = message.split('|')
  if (parts.length !== 4) {
    throw new Error(`rust driver: rejection without contract wire identity: ${message}`)
  }
  const [code, domain, operation, detail] = parts
  if (
    code === undefined ||
    code === '' ||
    domain === undefined ||
    domain === '' ||
    operation === undefined ||
    operation === '' ||
    detail === undefined ||
    detail === ''
  ) {
    throw new Error(`rust driver: rejection without contract wire identity: ${message}`)
  }
  return Object.freeze({ code, domain, operation, detail })
}

/** Captured outcome of a value-carrying probe (success or loud rejection). */
export type RustStringOutcome =
  | { readonly ok: true; readonly value: string }
  | { readonly ok: false; readonly error: RustWireError }

/** Captured outcome of a void probe (success or loud rejection). */
export type RustVoidOutcome = { readonly ok: true } | { readonly ok: false; readonly error: RustWireError }

/** Runs a value probe, capturing its contract identity on rejection. */
export function captureString(call: () => string): RustStringOutcome {
  try {
    return Object.freeze({ ok: true, value: call() })
  } catch (error) {
    return Object.freeze({ ok: false, error: parseRustWireError(error) })
  }
}

/** Runs a void probe, capturing its contract identity on rejection. */
export function captureVoid(call: () => void): RustVoidOutcome {
  try {
    call()
    return Object.freeze({ ok: true })
  } catch (error) {
    return Object.freeze({ ok: false, error: parseRustWireError(error) })
  }
}

/**
 * Thin driver over one native session. Must-succeed probes (`status`,
 * `sweep`, `destroy`) return raw values and let native rejections propagate
 * (a failure there is an unexpected regression, loud by construction).
 * Expected-rejection probes (`counter`, `bleTransition`) go through the
 * `capture*` helpers so tests can assert exact contract identities.
 */
export class RustBackendDriver {
  private readonly session: RustNativeSession

  constructor(addon: RustNativeAddon, revision: string) {
    this.session = new addon.EchoSession(revision)
  }

  /** Observes the session-owned REAL Central (frozen revision + counters). */
  status(): string {
    return this.session.centralStatus()
  }

  /** Drives a real kernel expiry sweep at decimal-string host time. */
  sweep(nowMsDecimal: string): string {
    return this.session.driveExpireSweep(nowMsDecimal)
  }

  /** Drives the real shutdown transition (`released` / `release-failed`). */
  destroy(): string {
    return this.session.driveDestroy()
  }

  /** Decimal-string u64 probe (DATA-02 mapping, lossless past 2^53). */
  counter(decimal: string): RustStringOutcome {
    return captureString(() => this.session.echoCounter(decimal))
  }

  /** Loud-rejection path for BLE transitions beyond the driven slice. */
  bleTransition(transition: string): RustVoidOutcome {
    return captureVoid(() => this.session.requestBleTransition(transition))
  }

  /**
   * Runs one scripted synthetic-radio staged step (a JSON object line)
   * against the session-owned staged transition core. Step-level core
   * rejections arrive as data (`ok:false` observations); only the session
   * lifetime rejects (captured for exact-identity assertions).
   */
  stagedStep(line: string): RustStringOutcome {
    return captureString(() => this.session.stagedStep(line))
  }

  /** Drains the staged observation log (FIFO, newline-joined JSON lines). */
  stagedDrainLog(): RustStringOutcome {
    return captureString(() => this.session.stagedDrainLog())
  }

  /** Observes the staged batch accounting as JSON. */
  stagedCounters(): RustStringOutcome {
    return captureString(() => this.session.stagedCounters())
  }

  /** Post-close staged probe (must reject `lifecycle.destroyed`). */
  postCloseStagedStep(line: string): RustStringOutcome {
    return captureString(() => this.session.stagedStep(line))
  }

  /** Destroys the binding lifetime. Idempotent. */
  close(): void {
    this.session.close()
  }

  /** Post-close counter probe (must reject `lifecycle.destroyed`). */
  postCloseCounter(decimal: string): RustStringOutcome {
    return captureString(() => this.session.echoCounter(decimal))
  }

  /** Post-close status probe (must reject `lifecycle.destroyed`). */
  postCloseStatus(): RustStringOutcome {
    return captureString(() => this.session.centralStatus())
  }

  /** Post-close transition probe (must reject `lifecycle.destroyed`). */
  postCloseTransition(transition: string): RustVoidOutcome {
    return captureVoid(() => this.session.requestBleTransition(transition))
  }
}
