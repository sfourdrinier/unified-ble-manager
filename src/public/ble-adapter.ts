// src/public/ble-adapter.ts

export interface BleAdapterState {
  readonly available: boolean
  readonly poweredOn: boolean
}

export interface BleAdapter {
  readonly id: string | null
  readonly state: BleAdapterState
}
