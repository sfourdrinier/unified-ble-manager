// src/public/stream-presets.ts

import { capacity, type Capacity } from '../backend-contract/primitives'
import { contractError } from '../backend-contract/errors'
import type { OverflowPolicy } from '../backend-contract/streams'
import type { PublicStreamOverflowPolicy } from './streams'

export type StreamPreset = 'latest' | 'balanced' | 'lossless-bounded' | 'custom'

export interface CustomStreamBudget {
  readonly itemCapacity: number
  readonly byteCapacity: number
  readonly reservedControlCapacity?: number
  readonly overflowPolicy?: PublicStreamOverflowPolicy
}

export type StreamPolicy =
  | Exclude<StreamPreset, 'custom'>
  | { readonly preset: 'custom'; readonly budget: CustomStreamBudget }

export interface StreamBudget {
  readonly itemCapacity: Capacity
  readonly byteCapacity: Capacity
  readonly reservedControlCapacity: Capacity
  readonly overflowPolicy: OverflowPolicy
}

export interface StreamPresetInput {
  readonly preset?: StreamPreset
  readonly custom?: Partial<StreamBudget> & { readonly overflowPolicy?: PublicStreamOverflowPolicy }
}

/**
 * Maps a public stream preset to exact bounded capacities.
 *
 * - `latest`: UI/status where newest value matters (bounded drop-oldest)
 * - `balanced`: normal scan/sensor notifications (bounded drop-oldest with visible notice)
 * - `lossless-bounded`: command/response that must fail rather than drop (terminal overflow)
 * - `custom`: caller-supplied budgets through `StreamPolicy`
 */
export function resolveStreamPreset(input: StreamPresetInput = {}): StreamBudget {
  const preset = input.preset ?? 'balanced'
  switch (preset) {
    case 'latest':
      return Object.freeze({
        itemCapacity: capacity(1),
        byteCapacity: capacity(4 * 1024),
        reservedControlCapacity: capacity(2),
        overflowPolicy: 'drop-oldest' as const
      })
    case 'balanced':
      return Object.freeze({
        itemCapacity: capacity(32),
        byteCapacity: capacity(16 * 1024),
        reservedControlCapacity: capacity(2),
        overflowPolicy: 'drop-oldest' as const
      })
    case 'lossless-bounded':
      return Object.freeze({
        itemCapacity: capacity(64),
        byteCapacity: capacity(64 * 1024),
        reservedControlCapacity: capacity(2),
        overflowPolicy: 'error' as const
      })
    case 'custom': {
      const custom = input.custom
      if (custom === undefined || custom.itemCapacity === undefined || custom.byteCapacity === undefined) {
        throw contractError('argument.invalid', 'stream', 'stream-preset.custom')
      }
      const reservedControlCapacity = custom.reservedControlCapacity ?? capacity(2)
      if (Number(custom.byteCapacity) <= Number(reservedControlCapacity)) {
        throw contractError('argument.invalid', 'stream', 'stream-preset.custom-byte-capacity')
      }
      return Object.freeze({
        itemCapacity: custom.itemCapacity,
        byteCapacity: custom.byteCapacity,
        reservedControlCapacity,
        overflowPolicy: custom.overflowPolicy ?? 'drop-oldest'
      })
    }
    default:
      throw contractError('argument.invalid', 'stream', 'stream-preset.unknown')
  }
}

export function resolveStreamPolicy(policy: StreamPolicy = 'balanced'): StreamBudget {
  if (typeof policy === 'string') return resolveStreamPreset({ preset: policy })
  if (policy.preset !== 'custom') {
    throw contractError('argument.invalid', 'stream', 'public-stream-policy.preset')
  }
  const budget = policy.budget
  const reservedControlCapacity = budget.reservedControlCapacity ?? 2
  if (
    !Number.isSafeInteger(budget.itemCapacity) ||
    budget.itemCapacity <= 0 ||
    !Number.isSafeInteger(budget.byteCapacity) ||
    budget.byteCapacity <= 0 ||
    !Number.isSafeInteger(reservedControlCapacity) ||
    reservedControlCapacity <= 0 ||
    budget.byteCapacity <= reservedControlCapacity ||
    (budget.overflowPolicy !== undefined &&
      budget.overflowPolicy !== 'latest' &&
      budget.overflowPolicy !== 'drop-oldest' &&
      budget.overflowPolicy !== 'drop-newest' &&
      budget.overflowPolicy !== 'error')
  ) {
    throw contractError('argument.invalid', 'stream', 'public-stream-policy.budget')
  }
  return resolveStreamPreset({
    preset: 'custom',
    custom: {
      itemCapacity: capacity(budget.itemCapacity),
      byteCapacity: capacity(budget.byteCapacity),
      reservedControlCapacity: capacity(reservedControlCapacity),
      overflowPolicy: budget.overflowPolicy
    }
  })
}

export const STREAM_PRESET_DEFAULTS = Object.freeze({
  scan: 'balanced' as StreamPreset,
  notification: 'balanced' as StreamPreset,
  indication: 'lossless-bounded' as StreamPreset
})
