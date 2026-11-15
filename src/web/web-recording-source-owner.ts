import type { WebBluetoothCharacteristicBoundary } from './web-bluetooth-boundary'
import type {
  WebRecordingMemberLease,
  WebRecordingSourceOwner as SupervisorSourceOwner
} from './web-ordered-recording-supervisor'
import {
  WebIngressRegistrationCleanupError,
  WebNotificationIngress,
  type WebRetainedIngressGroup,
  type WebRetainedIngressReservation
} from './web-notification-ingress'
import { contractError } from '../backend-contract/errors'
import { orderedSetupTerminalError } from './web-bluetooth-errors'
import { webRecordingContractError } from './web-recording-errors'
import type { CharacteristicPath } from '../backend-contract/gatt'
import type { SubscriptionOptions } from '../backend-contract/operations'
import { characteristicKey, type WebGattDatabase } from './web-bluetooth-handles'
import { assertWebDeliveryMode } from './web-delivery-mode'
import type { WebRetainedAcknowledgedDelivery } from '../core/web-retained-recording-pump'

/** A lease port must retain provisional and late enable effects, even when
 * enable rejects. Release must reconcile them and be retryable on failure. */
export interface WebRecordingSourceMember {
  readonly id: string
  readonly characteristic: WebBluetoothCharacteristicBoundary
  readonly lease: { enable(): Promise<void>; release(): Promise<void> }
}

export interface WebRecordingGraphMember {
  readonly id: string
  readonly path: CharacteristicPath<string, string, string, string, string, 'current'>
  readonly deliveryMode: NonNullable<SubscriptionOptions['deliveryMode']>
  readonly lease: WebRecordingSourceMember['lease']
}

/** Bind a supervisor owner to exact characteristic instances in a live graph.
 * The lease port must be provided by the GATT owner, not a second listener. */
export function createGraphBoundWebRecordingSourceOwner(
  database: WebGattDatabase,
  ingress: WebNotificationIngress,
  members: readonly WebRecordingGraphMember[],
  capacity: number,
  byteCapacity: number,
  hostCurrent: () => boolean,
  recordingVersion: 1 | 2 = 1
): WebRecordingSourceOwner {
  const operation = 'web-recording.source'
  const resolved = members.map(member => {
    database.assertPath(member.path, operation)
    if (!database.record.grantedServices.has(String(member.path.serviceUuid)))
      throw contractError('chooser.optional-service-not-granted', 'chooser', operation)
    const characteristic = database.characteristicBoundaries.get(characteristicKey(member.path))
    if (characteristic === undefined) throw contractError('gatt.stale-handle', 'gatt', operation)
    assertWebDeliveryMode(characteristic, member.deliveryMode, operation)
    return { id: member.id, characteristic, lease: member.lease, path: member.path }
  })
  const current = (): boolean => {
    if (
      !hostCurrent() ||
      !database.record.valid ||
      database.record.database !== database ||
      !database.record.device.gatt.connected
    )
      return false
    try {
      return resolved.every(member => {
        database.assertPath(member.path, operation)
        return (
          database.record.grantedServices.has(String(member.path.serviceUuid)) &&
          database.characteristicBoundaries.get(characteristicKey(member.path)) === member.characteristic
        )
      })
    } catch {
      return false
    }
  }
  return new WebRecordingSourceOwner(ingress, resolved, capacity, byteCapacity, current, recordingVersion)
}

/** The live supervisor integration point. The host supplies leases from the
 * same GATT owner that dispatches DOM callbacks to this ingress. */
export class WebRecordingSourceOwner implements SupervisorSourceOwner {
  private source: WebRetainedIngressGroup | null = null
  private reservation: WebRetainedIngressReservation | null = null
  private installationCleanup: WebIngressRegistrationCleanupError | null = null
  private released = false
  markerDebtPending: (() => boolean) | null = null
  private wakeSupervisor: (() => void) | null = null
  onMarkerEvidenceProgress: (() => void) | null = null
  private markerAcknowledgementFailed = false
  onAcknowledged: ((delivery: WebRetainedAcknowledgedDelivery) => void | Promise<void>) | null = null
  private readonly states: {
    dispatched: boolean
    released: boolean
    enabling: Promise<void> | null
    releasing: Promise<void> | null
  }[]

  constructor(
    private readonly ingress: WebNotificationIngress,
    private readonly members: readonly WebRecordingSourceMember[],
    private readonly capacity: number,
    private readonly byteCapacity: number,
    private readonly current: () => boolean,
    readonly recordingVersion: 1 | 2 = 1
  ) {
    if (recordingVersion !== 1 && recordingVersion !== 2)
      throw contractError('protocol.incompatible', 'boundary', 'web-recording.source.version')
    this.members = [...members]
    this.states = members.map(() => ({ dispatched: false, released: false, enabling: null, releasing: null }))
  }

  reserve(): { memberships(): readonly { readonly member: string; readonly epoch: string }[] } {
    if (
      this.reservation !== null ||
      this.source !== null ||
      this.released ||
      !this.isCurrent() ||
      this.members.length === 0 ||
      this.members.some(member => member.id.length === 0) ||
      new Set(this.members.map(member => member.id)).size !== this.members.length ||
      new Set(this.members.map(member => member.characteristic)).size !== this.members.length
    ) {
      throw new Error('Invalid Web recording source reservation')
    }
    const mapping = new Map(this.members.map((member, index) => [member.characteristic, String(index)]))
    this.reservation = this.ingress.reserveRetainedMemberships(
      mapping,
      this.capacity,
      this.byteCapacity,
      this.recordingVersion
    )
    const reservation = this.reservation
    return { memberships: () => reservation.memberships() }
  }

  install(wake: () => void): {
    readonly source: WebRetainedIngressGroup
    readonly leases: readonly WebRecordingMemberLease[]
  } {
    if (this.reservation === null || this.source !== null || this.released || !this.isCurrent())
      throw new Error('Invalid Web recording source installation')
    let source: WebRetainedIngressGroup
    try {
      source = this.reservation.install(wake)
    } catch (error) {
      if (error instanceof WebIngressRegistrationCleanupError) this.installationCleanup = error
      throw error
    }
    this.source = source
    this.wakeSupervisor = wake
    const unavailable = () =>
      source.terminalReason === null
        ? webRecordingContractError('gatt.stale-handle', 'gatt', 'web-recording.source')
        : orderedSetupTerminalError(source.terminalReason)
    const leases = this.members.map((member, index): WebRecordingMemberLease => {
      const state = this.states[index]
      if (state === undefined) throw new Error('Invalid Web recording source member')
      return {
        id: member.id,
        enable: () => {
          if (state.dispatched || state.released)
            return Promise.reject(webRecordingContractError('lifecycle.invalid-state', 'gatt', 'web-recording.source'))
          if (source.terminal || !this.isCurrent()) return Promise.reject(unavailable())
          state.dispatched = true
          const enabling = Promise.resolve().then(() => {
            if (source.terminal || !this.isCurrent()) throw unavailable()
            return member.lease.enable()
          })
          state.enabling = enabling
          return enabling
        },
        release: () => {
          if (state.released || !state.dispatched) return Promise.resolve()
          if (state.releasing !== null) return state.releasing
          const releasing = (async () => {
            try {
              await state.enabling
            } catch {
              /* release reconciles an ambiguous enable */
            }
            await member.lease.release()
            state.released = true
          })()
          state.releasing = releasing.then(
            () => {
              state.releasing = null
            },
            error => {
              state.releasing = null
              throw error
            }
          )
          return state.releasing
        }
      }
    })
    return { source, leases }
  }

  async release(): Promise<void> {
    if (this.released) return
    if (this.installationCleanup !== null) {
      try {
        this.installationCleanup.retry()
        this.installationCleanup = null
      } catch (error) {
        if (error instanceof WebIngressRegistrationCleanupError) this.installationCleanup = error
        throw error
      }
    }
    if (this.source === null) {
      this.released = true
      return
    }
    if (!this.source.terminal || this.states.some(state => state.dispatched && !state.released))
      throw new Error('Web recording source still owns deliveries or member leases')
    this.source.seal?.()
    if (this.source.peek() !== null) throw new Error('Web recording source still owns deliveries or member leases')
    await this.releaseRetainingDeliveries()
  }

  /** Detach the fenced ingress while its caller retains the source FIFO. */
  async releaseRetainingDeliveries(): Promise<void> {
    if (this.released) return
    if (
      this.source === null ||
      !this.source.terminal ||
      this.states.some(state => state.dispatched && !state.released)
    ) {
      throw new Error('Web recording source still owns deliveries or member leases')
    }
    this.source.seal?.()
    this.released = true
  }

  hasMarkerDebt(): boolean {
    return this.markerDebtPending?.() === true
  }

  wakeMarkerDebt(): void {
    this.wakeSupervisor?.()
    if (this.markerAcknowledgementFailed) this.onMarkerEvidenceProgress?.()
  }

  isCurrent(): boolean {
    return !this.released && this.current()
  }

  isInstalled(): boolean {
    return this.source !== null && !this.released
  }

  mayEnable(): boolean {
    return this.isInstalled() && this.source?.terminal === false && this.isCurrent()
  }

  installedSource(): WebRetainedIngressGroup | null {
    return this.source
  }

  acknowledged(delivery: WebRetainedAcknowledgedDelivery): void | Promise<void> {
    try {
      const result = this.onAcknowledged?.(delivery)
      if (delivery.record.kind === 'marker') {
        if (result === undefined) this.markerAcknowledgementFailed = false
        else
          result.then(
            () => {
              this.markerAcknowledgementFailed = false
            },
            () => {
              this.markerAcknowledgementFailed = true
            }
          )
      }
      return result
    } catch (error) {
      if (delivery.record.kind === 'marker') this.markerAcknowledgementFailed = true
      throw error
    }
  }

  memberships(): readonly { readonly member: string; readonly epoch: string }[] | null {
    return this.reservation?.memberships() ?? null
  }
}
