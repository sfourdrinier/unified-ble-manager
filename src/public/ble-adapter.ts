// src/public/ble-adapter.ts

import type { AdapterStateSnapshot } from '../backend-contract/identity'
import type { OperationOptions } from './operation-options'
import type { CleanupRecord } from './cleanup'
import type { PublicBoundedAsyncStream } from './streams'

export type BleAdapterState = Omit<AdapterStateSnapshot<string>, 'backendGeneration' | 'updatedAt'> & {
  readonly backendGeneration: string
  readonly updatedAt: number
}

export interface AdapterReadinessOptions extends OperationOptions {
  readonly operation?: 'scan' | 'choose' | 'connect' | 'known-peers'
}

export interface AdapterWatchOptions {
  readonly signal?: AbortSignal | null
}

export interface BleAdapterStateWatch {
  readonly initial: BleAdapterState
  readonly values: PublicBoundedAsyncStream<BleAdapterState>
  stop(): Promise<CleanupRecord>
}

export interface BleAdapter {
  readonly id: string | null
  state(): Promise<BleAdapterState>
  waitUntilReady(options?: AdapterReadinessOptions): Promise<BleAdapterState>
  watchState(options?: AdapterWatchOptions): Promise<BleAdapterStateWatch>
}
