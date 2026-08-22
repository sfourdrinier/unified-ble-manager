// src/backends/bluez/bluez-operation-dispatcher.ts

import { BackendContractError, contractError } from '../../backend-contract/errors'
import {
  createBackendOperationDispatch,
  type BackendOperationDispatch,
  type BackendOperationPhysicalSettlement,
  type CancellationAcknowledgement,
  type PublicOperationOptions
} from '../../backend-contract/operations'
import { opaqueId, type BackendOperationHandle, type SerializableRecord } from '../../backend-contract/primitives'
import { BluezDbusMethodError } from './bluez-dbus-contract'

interface ActiveBluezOperation {
  readonly handle: BackendOperationHandle<string, string>
  cancel(): CancellationAcknowledgement<string>
}

export interface BluezOperationDispatch<Result> extends BackendOperationDispatch<string, Result> {
  readonly physicalSettlement: BackendOperationPhysicalSettlement
}

export class BluezOperationDispatcher {
  private nextOperation = 1
  private readonly active = new Map<string, ActiveBluezOperation>()
  private readonly idleWaiters = new Set<() => void>()

  constructor(private readonly now: () => number) {}

  dispatch<Result>(
    options: PublicOperationOptions,
    operationName: string,
    operation: () => Promise<Result>,
    onCancellation?: () => Promise<void>
  ): BluezOperationDispatch<Result> {
    const handle = opaqueId(`bluez-operation-${this.nextOperation}`, 'backend-operation', 'bluez:dispatcher')
    this.nextOperation += 1
    let callerTerminal = false
    let operationSettled = false
    let retired = false
    let rejectCompletion: ((error: Error) => void) | null = null
    let deadlineTimer: ReturnType<typeof setTimeout> | null = null
    let cancellation: Promise<void> | null = null
    let resolvePhysicalSettlement: (() => void) | null = null
    const physicalSettlement = new Promise<void>(resolve => {
      resolvePhysicalSettlement = resolve
    })
    const requestPhysicalCancellation = (): Promise<void> => {
      if (operationSettled) return Promise.resolve()
      if (cancellation === null) {
        cancellation = onCancellation === undefined ? Promise.resolve() : Promise.resolve().then(onCancellation)
      }
      return cancellation
    }
    const abort = (): void => {
      if (callerTerminal) {
        return
      }
      callerTerminal = true
      clearAdmission()
      rejectCompletion?.(contractError('operation.aborted', 'core', operationName))
      requestPhysicalCancellation().catch(() => undefined)
    }
    const clearAdmission = (): void => {
      if (deadlineTimer !== null) {
        clearTimeout(deadlineTimer)
        deadlineTimer = null
      }
      options.signal?.removeEventListener('abort', abort)
    }
    const settlePhysical = (): void => {
      if (operationSettled) {
        return
      }
      operationSettled = true
      clearAdmission()
      const retire = (): void => {
        if (retired) return
        retired = true
        this.active.delete(String(handle))
        if (resolvePhysicalSettlement === null) {
          throw new Error('BlueZ physical settlement resolver was not initialized')
        }
        resolvePhysicalSettlement()
        if (this.active.size === 0) {
          for (const resolve of this.idleWaiters) {
            resolve()
          }
          this.idleWaiters.clear()
        }
      }
      if (cancellation === null) retire()
      else cancellation.then(retire, retire)
    }
    const completion = new Promise<Result>((resolve, reject) => {
      rejectCompletion = reject
      if (options.signal?.aborted === true) {
        callerTerminal = true
        reject(contractError('operation.aborted', 'core', operationName))
        settlePhysical()
        return
      }
      if (options.deadline !== null && options.deadline <= this.now()) {
        callerTerminal = true
        reject(contractError('operation.timed-out', 'core', operationName))
        settlePhysical()
        return
      }
      options.signal?.addEventListener('abort', abort, { once: true })
      if (options.deadline !== null) {
        deadlineTimer = setTimeout(
          () => {
            if (callerTerminal) {
              return
            }
            callerTerminal = true
            clearAdmission()
            reject(contractError('operation.timed-out', 'core', operationName))
            requestPhysicalCancellation().catch(() => undefined)
          },
          Math.max(0, options.deadline - this.now())
        )
      }
      Promise.resolve()
        .then(() => {
          if (callerTerminal) {
            throw contractError('operation.aborted', 'core', operationName)
          }
          return operation()
        })
        .then(
          result => {
            settlePhysical()
            if (callerTerminal) {
              return
            }
            callerTerminal = true
            resolve(result)
          },
          error => {
            settlePhysical()
            if (callerTerminal) {
              return
            }
            callerTerminal = true
            if (error instanceof BackendContractError) {
              reject(error)
              return
            }
            if (error instanceof BluezDbusMethodError) {
              reject(
                contractError(
                  'platform.failure',
                  'platform',
                  operationName,
                  Object.freeze({
                    domain: 'bluez-dbus',
                    code: error.detail.name,
                    safeMessage: error.detail.message,
                    metadata: dbusSafeMetadata(error)
                  })
                )
              )
              return
            }
            const safeMessage = error instanceof Error ? error.message : 'D-Bus rejected with a non-Error value'
            reject(
              contractError(
                'platform.failure',
                'platform',
                operationName,
                Object.freeze({
                  domain: 'bluez-dbus',
                  code: 'org.bluez.Error.Failed',
                  safeMessage,
                  metadata: Object.freeze({})
                })
              )
            )
          }
        )
    })
    const active: ActiveBluezOperation = {
      handle,
      cancel: () => {
        if (operationSettled) {
          return { handle, state: 'already-terminal' }
        }
        abort()
        return { handle, state: 'not-cancellable' }
      }
    }
    if (!operationSettled) {
      this.active.set(String(handle), active)
    }
    const dispatch = createBackendOperationDispatch(
      handle,
      completion,
      async () => {
        const acknowledgement = active.cancel()
        await requestPhysicalCancellation()
        return acknowledgement
      },
      physicalSettlement
    )
    return { ...dispatch, physicalSettlement }
  }

  cancelAll(): void {
    for (const active of [...this.active.values()]) {
      active.cancel()
    }
  }

  activeCount(): number {
    return this.active.size
  }

  waitForIdle(): Promise<void> {
    if (this.active.size === 0) {
      return Promise.resolve()
    }
    return new Promise(resolve => {
      this.idleWaiters.add(resolve)
    })
  }
}

function dbusSafeMetadata(error: BluezDbusMethodError): SerializableRecord {
  const metadata: Record<string, string | boolean | number> = {}
  for (const [key, variant] of Object.entries(error.detail.safeDetails)) {
    if (typeof variant.value === 'string' || typeof variant.value === 'boolean' || typeof variant.value === 'number') {
      metadata[key] = variant.value
    }
  }
  return Object.freeze(metadata)
}
