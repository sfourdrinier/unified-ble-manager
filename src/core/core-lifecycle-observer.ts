// src/core/core-lifecycle-observer.ts

import { BackendContractError, contractError } from '../backend-contract/errors'
import type { CleanupFailure, CleanupRecord } from '../backend-contract/errors'
import { CoreTraceRecorder } from './trace-recorder'
import type { CoreTraceResource } from './trace-recorder'

export interface RetainedCleanup {
  readonly resourceKind: CoreTraceResource
  readonly transition: string
  retry(): Promise<CleanupRecord>
}

/** Records asynchronous lifecycle cleanup without allowing background failures to escape unchecked. */
export class CoreLifecycleObserver {
  private readonly retainedCleanups = new Set<RetainedCleanup>()

  constructor(
    private readonly trace: CoreTraceRecorder,
    private readonly now: () => number
  ) {}

  retainCleanup(cleanup: RetainedCleanup): void {
    this.retainedCleanups.add(cleanup)
  }

  dropCleanup(cleanup: RetainedCleanup): void {
    this.retainedCleanups.delete(cleanup)
  }

  observeCleanup(cleanup: Promise<CleanupRecord>, transition: string): void {
    cleanup.then(
      result => {
        if (result.state === 'release-failed') {
          this.record('database', transition, result.failures[0]?.error.code ?? 'platform.failure')
        }
      },
      () => this.record('database', `${transition}-rejected`, 'platform.failure')
    )
  }

  observeBackground(task: Promise<void>, resource: CoreTraceResource, transition: string): void {
    task.then(
      () => undefined,
      error =>
        this.record(
          resource,
          transition,
          error instanceof BackendContractError ? error.normalized.code : 'platform.failure'
        )
    )
  }

  async captureCleanup(
    cleanup: Promise<CleanupRecord>,
    resourceKind: CoreTraceResource,
    transition: string
  ): Promise<CleanupRecord> {
    try {
      return await cleanup
    } catch (error) {
      const cause = error instanceof BackendContractError ? error.normalized.code : 'platform.failure'
      this.record(resourceKind, transition, cause)
      return {
        state: 'release-failed',
        failures: [
          {
            resourceKind,
            error: contractError('platform.failure', 'cleanup', `unified-core.${transition}`).normalized
          }
        ]
      }
    }
  }

  async retryRetainedCleanups(): Promise<CleanupRecord> {
    const failures: CleanupFailure[] = []
    for (const cleanup of [...this.retainedCleanups]) {
      const result = await this.captureCleanup(cleanup.retry(), cleanup.resourceKind, cleanup.transition)
      if (result.state === 'released') {
        this.retainedCleanups.delete(cleanup)
      } else {
        failures.push(...result.failures)
      }
    }
    return failures.length === 0 ? { state: 'released', failures: [] } : { state: 'release-failed', failures }
  }

  private record(
    resource: CoreTraceResource,
    transition: string,
    cause: import('../backend-contract/errors').BleErrorCode
  ): void {
    this.trace.record({
      timestamp: this.now(),
      resource,
      transition,
      operation: null,
      cause,
      queuedOperations: 0,
      dispatchedOperations: 0,
      quarantinedOperations: 0
    })
  }
}
