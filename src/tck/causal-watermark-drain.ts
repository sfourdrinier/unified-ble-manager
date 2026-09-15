// src/tck/causal-watermark-drain.ts
//
// Causal-watermark drain helpers (UBM 5.0 TCK card, Phase 2 work item 5).
// Integration tests acknowledge a causal sequence/watermark of submitted work.
// They never wait for global idleness while a scan or stream is intentionally
// live. These helpers track submission order and drain only the causally
// submitted prefix via bounded controller flushes.

import type { TckScenarioController } from './contracts'

export interface CausalWatermark {
  readonly sequence: number
  readonly submitted: readonly string[]
  markSubmitted(label: string): number
}

export interface WatermarkDrainReceipt {
  readonly drainedToSequence: number
  readonly drainedLabels: readonly string[]
  readonly globalIdlenessWaited: boolean
}

interface MutableWatermarkState {
  sequence: number
  submitted: string[]
}

export function createCausalWatermark(): CausalWatermark {
  const state: MutableWatermarkState = { sequence: 0, submitted: [] }
  return {
    get sequence(): number {
      return state.sequence
    },
    get submitted(): readonly string[] {
      return Object.freeze([...state.submitted])
    },
    markSubmitted(label: string): number {
      if (label.length === 0) {
        throw new Error('causal-watermark: submission label must not be empty')
      }
      state.sequence += 1
      state.submitted.push(label)
      return state.sequence
    }
  }
}

/**
 * Drains only work submitted up to the watermark's current sequence. The
 * causally-submitted promises are observed (settledness flags, no floating
 * promises), given bounded microtask flushes tied to the submitted depth, and
 * then actually settled through the controller one by one. Never performs a
 * global idle drain, so an intentionally live scan or stream stays open.
 * Fail-closed: work that cannot settle rejects here instead of racing a
 * downstream assertion.
 */
export async function drainToWatermark(
  controller: TckScenarioController,
  watermark: CausalWatermark,
  causallySubmitted: readonly Promise<unknown>[]
): Promise<WatermarkDrainReceipt> {
  const target = watermark.sequence
  const labels = [...watermark.submitted]
  const tracked = causallySubmitted.map(promise => {
    let settled = false
    const observed = promise.then(
      () => {
        settled = true
      },
      () => {
        settled = true
      }
    )
    return {
      isSettled: (): boolean => settled,
      observed
    }
  })
  const flushBound = causallySubmitted.length + 1
  for (let turn = 0; turn < flushBound; turn += 1) {
    if (tracked.every(entry => entry.isSettled())) {
      break
    }
    await controller.flush()
  }
  for (const entry of tracked) {
    await controller.settle(entry.observed)
  }
  return Object.freeze({
    drainedToSequence: target,
    drainedLabels: Object.freeze(labels),
    globalIdlenessWaited: false
  })
}
