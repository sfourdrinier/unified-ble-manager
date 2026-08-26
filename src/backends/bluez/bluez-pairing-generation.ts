// src/backends/bluez/bluez-pairing-generation.ts
//
// Selecting the LE pairing generation on Linux, for the peripherals that only
// accept LE Legacy and terminate the link on an LE Secure Connections pairing
// request.
//
// WHY THIS NEEDS A HOST-SUPPLIED OPERATION. `org.bluez.Adapter1` exposes no
// pairing-generation property in BlueZ 5.85 - verified against a live daemon,
// not inferred - and `org.bluez.Device1.Pair` takes no parameters. The setting
// lives behind the kernel management socket's Set Secure Connections command,
// which requires CAP_NET_ADMIN. There is no unprivileged route.
//
// So this package does not reach for one. It never opens a management socket,
// never shells out to `btmgmt`, and never assumes it is already root: a library
// that silently acquires privilege gives an application capabilities its author
// did not choose and cannot audit. Instead the HOST supplies the privileged
// operation, which makes the escalation visible in the application that opted
// into it, and leaves the default posture of this backend exactly as it was.
//
// WHAT A HOST MUST UNDERSTAND BEFORE SUPPLYING ONE. The setting is
// ADAPTER-WIDE, not per-pairing: while it is in force, every pairing on that
// controller uses the selected generation, including pairings this package did
// not initiate. It also OUTLIVES the process that set it - the kernel keeps it
// until something sets it back. This module therefore restores the previous
// value after each pairing and reports a failed restore rather than swallowing
// it, because a controller silently left in LE Legacy is a security regression
// that no later operation would explain.

import { type SerializableRecord } from '../../backend-contract/primitives'

/**
 * The adapter-wide pairing generation, in the kernel's own terms.
 *
 * - `'legacy-only'` - Secure Connections off; the controller pairs LE Legacy.
 * - `'enabled'`     - Secure Connections used when the peer supports it.
 * - `'required'`    - Secure Connections only; a peer that cannot is refused.
 */
export type BluezPairingGeneration = 'legacy-only' | 'enabled' | 'required'

/**
 * A privileged operation supplied by the host.
 *
 * Implement it with the kernel management socket's Set Secure Connections
 * command (`CAP_NET_ADMIN`), or a helper process that holds that capability.
 * `read` must report the adapter's current value so it can be restored.
 */
export interface BluezPairingGenerationController {
  read(adapterId: string): Promise<BluezPairingGeneration>
  set(adapterId: string, generation: BluezPairingGeneration): Promise<void>
}

/**
 * The kernel generation a public `secureConnections` request maps to.
 *
 * `'prefer'` is deliberately not accepted: it means defer to the platform, so
 * it must never touch adapter-wide state even when a controller is available.
 * Excluding it from the parameter type makes that a compile-time fact rather
 * than a rule someone has to remember.
 */
export function generationForSecureConnections(secureConnections: 'require' | 'disallow'): BluezPairingGeneration {
  return secureConnections === 'require' ? 'required' : 'legacy-only'
}

/**
 * Reports a restore that failed. The adapter is left in the wrong generation,
 * which weakens or breaks every later pairing on the host, so it must reach
 * someone even when the operation it accompanied succeeded.
 */
export type PairingGenerationRestoreReporter = (failure: {
  readonly adapterId: string
  readonly heldGeneration: BluezPairingGeneration
  readonly intendedGeneration: BluezPairingGeneration
  readonly detail: string
}) => void

const adapterQueues = new Map<string, Promise<unknown>>()

/**
 * Serialise the read/set/restore sandwich per adapter.
 *
 * The setting is adapter-wide, so two overlapping pairings on one controller
 * interleave: the second reads the first's temporary value as "previous" and
 * restores to it, leaving the adapter wrong whichever finishes last. Peer-level
 * arbitration cannot see this - the peers differ, the adapter does not.
 */
async function onAdapter<Result>(adapterId: string, work: () => Promise<Result>): Promise<Result> {
  const previous = adapterQueues.get(adapterId) ?? Promise.resolve()
  const run = previous.then(work, work)
  adapterQueues.set(
    adapterId,
    run.then(
      () => undefined,
      () => undefined
    )
  )
  try {
    return await run
  } finally {
    if (adapterQueues.get(adapterId) === run) adapterQueues.delete(adapterId)
  }
}

/**
 * Run `pair` with the adapter held at `generation`, then put the adapter back.
 *
 * The restore runs whether the pairing succeeded or failed, and a restore that
 * fails is never dropped. What it must NOT do is change the pairing's outcome:
 * a bond that was actually created is never reported as if it never happened,
 * which is the rule this backend already follows for a late abort. So:
 *
 * - pairing succeeded, restore failed - the pairing SUCCEEDS. The restore
 *   failure goes to the reporter, because the caller asked whether they are
 *   bonded and the answer is yes; an adapter left misconfigured is a separate
 *   fact needing a separate channel, not a reason to deny a bond that exists.
 * - pairing failed, restore failed - the PAIRING error is thrown, with the
 *   restore failure attached to it. The pairing error is the one they asked
 *   about; reducing it to a boolean on a restore error inverts that.
 */
function isPairingGeneration(value: unknown): value is BluezPairingGeneration {
  return value === 'legacy-only' || value === 'enabled' || value === 'required'
}

/**
 * Put the adapter back, reporting rather than throwing if it will not go.
 *
 * Returns the failure detail, or `null` when the adapter was restored. Never
 * throws: a restore failure must not become the caller's outcome, and neither
 * must a reporter that throws - a host whose logger is on a closed stderr must
 * not be able to turn a completed bond into a reported failure.
 */
async function restore(
  controller: BluezPairingGenerationController,
  adapterId: string,
  previous: BluezPairingGeneration,
  held: BluezPairingGeneration,
  report: PairingGenerationRestoreReporter
): Promise<string | null> {
  try {
    await controller.set(adapterId, previous)
    return null
  } catch (error) {
    const detail = error instanceof Error ? error.message : String(error)
    try {
      report({ adapterId, heldGeneration: held, intendedGeneration: previous, detail })
    } catch {
      // A reporter that throws is a host defect. It must not decide the
      // operation's outcome, so it is contained here.
    }
    return detail
  }
}

export async function withPairingGeneration<Result>(
  controller: BluezPairingGenerationController,
  adapterId: string,
  generation: BluezPairingGeneration,
  pair: () => Promise<Result>,
  reportRestoreFailure: PairingGenerationRestoreReporter
): Promise<Result> {
  return onAdapter(adapterId, async () => {
    const previous = await controller.read(adapterId)
    if (!isPairingGeneration(previous)) {
      throw new TypeError(
        `pairing-generation controller returned ${JSON.stringify(previous)}; ` +
          "expected 'legacy-only', 'enabled' or 'required'"
      )
    }
    const held = previous !== generation
    if (held) {
      try {
        await controller.set(adapterId, generation)
      } catch (error) {
        // The command may have half-applied - reached the kernel, then failed
        // on the response - so the adapter cannot be assumed untouched. Try to
        // put it back before giving up, and report if that fails too. Treating
        // a failed `set` as "nothing happened" is how a controller ends up
        // silently stuck in LE Legacy.
        await restore(controller, adapterId, previous, generation, reportRestoreFailure)
        throw error
      }
    }

    let outcome: { readonly ok: true; readonly value: Result } | { readonly ok: false; readonly error: unknown }
    try {
      outcome = { ok: true, value: await pair() }
    } catch (error) {
      outcome = { ok: false, error }
    }

    const restoreDetail = held ? await restore(controller, adapterId, previous, generation, reportRestoreFailure) : null

    if (outcome.ok) return outcome.value
    throw restoreDetail === null
      ? outcome.error
      : attachRestoreFailure(outcome.error, adapterId, generation, previous, restoreDetail)
  })
}

/**
 * Carry the restore failure on the pairing error rather than instead of it.
 * A caller handling a pairing failure must still learn the adapter was left
 * misconfigured, without the pairing's own reason being displaced.
 */
function attachRestoreFailure(
  pairingError: unknown,
  adapterId: string,
  heldGeneration: BluezPairingGeneration,
  intendedGeneration: BluezPairingGeneration,
  detail: string
): unknown {
  if (!(pairingError instanceof Error)) return pairingError
  const annotated: Error & { pairingGenerationRestoreFailure?: SerializableRecord } = pairingError
  annotated.pairingGenerationRestoreFailure = Object.freeze({
    adapterId,
    heldGeneration,
    intendedGeneration,
    detail
  })
  return annotated
}
