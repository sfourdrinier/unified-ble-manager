import type { AdapterStateSnapshot } from '../backend-contract/identity'
import type { OperationOptions } from './operation-options'

export type BleAdapterState = Omit<AdapterStateSnapshot<string>, 'backendGeneration' | 'updatedAt'> & {
  readonly backendGeneration: string
  readonly updatedAt: number
}

export interface AdapterReadinessOptions extends OperationOptions {
  readonly operation?: 'scan' | 'choose' | 'connect' | 'known-peers'
}

export interface BleAdapter {
  readonly id: string | null
  state(): Promise<BleAdapterState>
  waitUntilReady(options?: AdapterReadinessOptions): Promise<BleAdapterState>
}
