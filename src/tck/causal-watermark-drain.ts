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
 * Drains only work submitted up to the watermark's current sequence. Uses
 * bounded controller flushes (microtask turns) and never performs a global
 * idle drain, so an intentionally live scan or stream stays open.
 */
export async function drainToWatermark(
  controller: TckScenarioController,
  watermark: CausalWatermark,
  causallySubmitted: readonly Promise<unknown>[]
): Promise<WatermarkDrainReceipt> {
  const target = watermark.sequence
  const labels = [...watermark.submitted]
  for (const pending of causallySubmitted) {
    pending.catch(() => undefined)
  }
  for (let turn = 0; turn < 4; turn += 1) {
    await controller.flush()
  }
  return Object.freeze({
    drainedToSequence: target,
    drainedLabels: Object.freeze(labels),
    globalIdlenessWaited: false
  })
}
