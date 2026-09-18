// examples-shared/driver/user-gesture.ts
//
// Web Bluetooth opens its chooser only inside a user activation. A remote
// command cannot produce one, so the scenario parks in an explicit
// "pending user gesture" state that the page shows as a button and the
// driver reports as a phase and an event. Nothing here synthesizes a gesture:
// the only way forward is a real click calling `grant`.

import { ScenarioError } from './scenario-core.ts'

export interface UserGestureRequest {
  readonly id: number
  readonly scenario: string
  readonly reason: string
}

export interface UserGestureGate {
  /** Resolves when a person grants this request; rejects when `signal` aborts first. */
  request(scenario: string, reason: string, signal: AbortSignal): Promise<void>
}

type Pending = UserGestureRequest & { readonly resolve: () => void }

export class PendingUserGestureGate implements UserGestureGate {
  private pendingRequests: Pending[] = []
  private readonly listeners = new Set<(pending: readonly UserGestureRequest[]) => void>()
  private nextId = 1

  request(scenario: string, reason: string, signal: AbortSignal): Promise<void> {
    if (signal.aborted) return Promise.reject(aborted(scenario))
    return new Promise((resolve, reject) => {
      const id = this.nextId++
      const onAbort = () => {
        this.remove(id)
        reject(aborted(scenario))
      }
      signal.addEventListener('abort', onAbort, { once: true })
      this.pendingRequests = [
        ...this.pendingRequests,
        {
          id,
          scenario,
          reason,
          resolve: () => {
            signal.removeEventListener('abort', onAbort)
            resolve()
          }
        }
      ]
      this.notify()
    })
  }

  pending(): readonly UserGestureRequest[] {
    return this.pendingRequests.map(({ id, scenario, reason }) => ({ id, scenario, reason }))
  }

  subscribe(listener: (pending: readonly UserGestureRequest[]) => void): () => void {
    this.listeners.add(listener)
    return () => {
      this.listeners.delete(listener)
    }
  }

  /** Call from the click handler itself, so the continuation runs within that activation. */
  grant(id: number): void {
    const entry = this.pendingRequests.find(candidate => candidate.id === id)
    if (entry === undefined) throw new ScenarioError('host.user-gesture-unknown', `no pending user-gesture request #${id.toString()}`)
    this.remove(id)
    entry.resolve()
  }

  private remove(id: number): void {
    this.pendingRequests = this.pendingRequests.filter(candidate => candidate.id !== id)
    this.notify()
  }

  private notify(): void {
    const snapshot = this.pending()
    for (const listener of this.listeners) listener(snapshot)
  }
}

function aborted(scenario: string): ScenarioError {
  return new ScenarioError('operation.aborted', `${scenario}: run aborted while waiting for a user gesture`)
}
