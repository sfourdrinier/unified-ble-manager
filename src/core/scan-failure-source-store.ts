import { contractError } from '../backend-contract/errors'

export interface ScanFailureIdentity {
  readonly source: string
  readonly generation: string
}

export interface ScanFailureSourceHandle {
  readonly identity: ScanFailureIdentity
  readonly byteLength: number
  read(offset: number, maxBytes: number): Uint8Array
  release(): void
}

export interface ScanFailureSourceReservation {
  capture(input: Uint8Array): readonly ScanFailureSourceHandle[]
  close(): void
}

interface ScanFailureSourceLimits {
  readonly byteCapacity: number
  readonly occurrenceCapacity: number
  readonly recipientCapacity: number
  readonly chunkCapacity: number
}

/**
 * Bounded in-process storage for already encoded evidence. The byte limit is
 * reserved and allocated before capture; record and recipient limits bound
 * bookkeeping separately. This owner does not encode input or retain native
 * data. Callers own and account for those representations until capture.
 */
export class ScanFailureSourceStore {
  private readonly limits: ScanFailureSourceLimits
  private bytes = 0
  private occurrences = 0
  private recipients = 0
  private identityBytes = 0

  constructor(limits: ScanFailureSourceLimits) {
    for (const limit of [
      limits.byteCapacity,
      limits.occurrenceCapacity,
      limits.recipientCapacity,
      limits.chunkCapacity
    ]) {
      if (!Number.isSafeInteger(limit) || limit < 1) {
        throw contractError('argument.invalid', 'stream', 'scan-failure-source.capacity')
      }
    }
    this.limits = Object.freeze({
      byteCapacity: limits.byteCapacity,
      occurrenceCapacity: limits.occurrenceCapacity,
      recipientCapacity: limits.recipientCapacity,
      chunkCapacity: limits.chunkCapacity
    })
  }

  inspect(): Readonly<{
    reservedBytes: number
    occurrences: number
    recipients: number
    identityBytes: number
    limits: ScanFailureSourceLimits
  }> {
    return Object.freeze({
      reservedBytes: this.bytes,
      occurrences: this.occurrences,
      recipients: this.recipients,
      identityBytes: this.identityBytes,
      limits: Object.freeze({ ...this.limits })
    })
  }

  reserve(
    identity: ScanFailureIdentity,
    maximumBytes: number,
    recipientCount: number
  ): ScanFailureSourceReservation | null {
    if (
      typeof identity.source !== 'string' ||
      identity.source.length === 0 ||
      identity.source.length > 256 ||
      typeof identity.generation !== 'string' ||
      identity.generation.length === 0 ||
      identity.generation.length > 256 ||
      !Number.isSafeInteger(maximumBytes) ||
      maximumBytes < 1 ||
      !Number.isSafeInteger(recipientCount) ||
      recipientCount < 1
    )
      throw contractError('argument.invalid', 'stream', 'scan-failure-source.reservation')
    if (
      maximumBytes > this.limits.byteCapacity - this.bytes ||
      this.occurrences === this.limits.occurrenceCapacity ||
      recipientCount > this.limits.recipientCapacity - this.recipients
    )
      return null

    // Allocate before reporting successful admission. Never retain the input's
    // potentially larger backing buffer or any structured error object.
    let buffer: Uint8Array | null = new Uint8Array(maximumBytes)
    const ownerIdentity = Object.freeze({ source: identity.source, generation: identity.generation })
    const identityCharge = 2 * (ownerIdentity.source.length + ownerIdentity.generation.length)
    let state: 'reserved' | 'captured' | 'closed' = 'reserved'
    let remaining = recipientCount
    let length = 0
    const releaseOwner = (): void => {
      state = 'closed'
      buffer = null
      this.bytes -= maximumBytes
      this.identityBytes -= identityCharge
      this.occurrences--
    }
    // Create the bounded recipient set before admission and outside capture's
    // scope, so its closures do not retain the producer's input buffer.
    const handles = Object.freeze(
      Array.from({ length: recipientCount }, () => {
        let released = false
        return Object.freeze({
          identity: ownerIdentity,
          get byteLength() {
            return length
          },
          read: (offset: number, maxBytes: number): Uint8Array => {
            if (released || state !== 'captured' || buffer === null)
              throw contractError('lifecycle.invalid-state', 'stream', 'scan-failure-source.released')
            if (
              !Number.isSafeInteger(offset) ||
              offset < 0 ||
              offset > length ||
              !Number.isSafeInteger(maxBytes) ||
              maxBytes < 1 ||
              maxBytes > this.limits.chunkCapacity
            )
              throw contractError('argument.invalid', 'stream', 'scan-failure-source.read')
            return buffer.slice(offset, Math.min(length, offset + maxBytes))
          },
          release: (): void => {
            if (released) return
            released = true
            this.recipients--
            remaining--
            if (remaining === 0) releaseOwner()
          }
        })
      })
    )
    const reservation = Object.freeze({
      capture: (input: Uint8Array): readonly ScanFailureSourceHandle[] => {
        if (state !== 'reserved')
          throw contractError('lifecycle.invalid-state', 'stream', 'scan-failure-source.capture')
        if (typeof SharedArrayBuffer !== 'undefined' && input.buffer instanceof SharedArrayBuffer) {
          throw contractError('argument.invalid', 'stream', 'scan-failure-source.shared-memory')
        }
        if (input.byteLength > maximumBytes)
          throw contractError('stream.quota', 'stream', 'scan-failure-source.capture')
        if (buffer === null) throw contractError('lifecycle.invariant-violation', 'stream', 'scan-failure-source.owner')
        buffer.set(input)
        length = input.byteLength
        state = 'captured'
        return handles
      },
      close: (): void => {
        if (state === 'closed') return
        if (state !== 'reserved') throw contractError('lifecycle.invalid-state', 'stream', 'scan-failure-source.close')
        this.recipients -= remaining
        remaining = 0
        releaseOwner()
      }
    })
    this.bytes += maximumBytes
    this.identityBytes += identityCharge
    this.occurrences++
    this.recipients += recipientCount
    return reservation
  }
}
