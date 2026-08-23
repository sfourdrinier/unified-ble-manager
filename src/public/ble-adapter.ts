import type { AdapterStateSnapshot } from '../backend-contract/identity'
import type { CleanupRecord } from '../backend-contract/errors'
import type { StreamItem } from '../backend-contract/streams'
import type { OperationOptions } from './operation-options'

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
  readonly values: AsyncIterable<StreamItem<BleAdapterState>>
  stop(): Promise<CleanupRecord>
}

export interface BleAdapter {
  readonly id: string | null
  state(): Promise<BleAdapterState>
  waitUntilReady(options?: AdapterReadinessOptions): Promise<BleAdapterState>
  watchState(options?: AdapterWatchOptions): Promise<BleAdapterStateWatch>
}
