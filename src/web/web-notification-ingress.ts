// src/web/web-notification-ingress.ts

import type { WebBluetoothCharacteristicBoundary } from './web-bluetooth-boundary'

/** Internal source records; the eventual group operation owns translation and cleanup. */
export type WebIngressTerminalReason =
  | 'closed'
  | 'owner-released'
  | 'connection-lost'
  | 'source-failed'
  | 'service-changed'
  | 'operation-aborted'
  | 'operation-timed-out'
  | 'overflow'
  | 'ordinal-exhausted'

export type WebIngressRecord =
  | { readonly kind: 'value'; readonly memberId: string; readonly ordinal: string; readonly value: Uint8Array }
  | { readonly kind: 'loss'; readonly lostValues: number; readonly lostBytes: number }
  | {
      readonly kind: 'terminal'
      readonly reason: WebIngressTerminalReason
      readonly uncertainTail: boolean
    }

export interface WebIngressGroup {
  readonly terminal: boolean
  readonly terminalReason: WebIngressTerminalReason | null
  drain(): readonly WebIngressRecord[]
  retire(reason: WebIngressTerminalReason): void
  onTerminal(callback: () => void): void
}

interface GroupState {
  readonly members: ReadonlyMap<WebBluetoothCharacteristicBoundary, string>
  readonly sink: ((memberId: string, ordinal: string, value: Uint8Array) => boolean) | null
  readonly capacity: number
  readonly byteCapacity: number
  readonly records: WebIngressRecord[]
  bytes: number
  terminal: boolean
  terminalReason: WebIngressTerminalReason | null
  terminalCallback: (() => void) | null
}

const MAX_ORDINAL = 18446744073709551615n

/** Synchronous DOM callbacks, group registration, and retirement share this owner. */
export class WebNotificationIngress {
  private readonly groups = new Set<GroupState>()
  private ordinal = 0n
  private retired = false

  register(
    members: ReadonlyMap<WebBluetoothCharacteristicBoundary, string>,
    capacity: number,
    byteCapacity: number,
    sink: ((memberId: string, ordinal: string, value: Uint8Array) => boolean) | null = null
  ): WebIngressGroup {
    if (
      this.retired ||
      members.size === 0 ||
      !Number.isSafeInteger(capacity) ||
      capacity < 1 ||
      !Number.isSafeInteger(byteCapacity) ||
      byteCapacity < 0 ||
      new Set(members.values()).size !== members.size
    ) {
      throw new Error('Invalid or retired Web notification ingress group')
    }
    const group: GroupState = {
      members: new Map(members),
      sink,
      capacity,
      byteCapacity,
      records: [],
      bytes: 0,
      terminal: false,
      terminalReason: null,
      terminalCallback: null
    }
    this.groups.add(group)
    return {
      get terminal() {
        return group.terminal
      },
      get terminalReason() {
        return group.terminalReason
      },
      drain: () => {
        const records = group.records.splice(0)
        group.bytes = 0
        return records
      },
      retire: reason => this.finish(group, reason, true),
      onTerminal: callback => {
        group.terminalCallback = callback
      }
    }
  }

  admit(characteristic: WebBluetoothCharacteristicBoundary, value: Uint8Array): void {
    if (this.retired) return
    if (this.ordinal === MAX_ORDINAL) {
      for (const group of this.groups) this.finish(group, 'ordinal-exhausted', true)
      this.retired = true
      return
    }
    this.ordinal += 1n
    const ordinal = this.ordinal.toString()
    for (const group of this.groups) {
      const memberId = group.members.get(characteristic)
      if (memberId === undefined) continue
      if (group.sink !== null) {
        if (!group.sink(memberId, ordinal, value)) this.finish(group, 'overflow', true)
        continue
      }
      if (group.records.length >= group.capacity || group.bytes + value.byteLength > group.byteCapacity) {
        group.records.push({ kind: 'loss', lostValues: 1, lostBytes: value.byteLength })
        this.finish(group, 'overflow', true)
        continue
      }
      const bytes = new Uint8Array(value)
      group.records.push({ kind: 'value', memberId, ordinal, value: bytes })
      group.bytes += bytes.byteLength
    }
  }

  retire(reason: WebIngressTerminalReason): void {
    if (this.retired) return
    this.retired = true
    for (const group of this.groups) this.finish(group, reason, true)
  }

  private finish(group: GroupState, reason: WebIngressTerminalReason, uncertainTail: boolean): void {
    if (group.terminal) return
    group.terminal = true
    group.terminalReason = reason
    group.records.push({ kind: 'terminal', reason, uncertainTail })
    this.groups.delete(group)
    group.terminalCallback?.()
  }
}
