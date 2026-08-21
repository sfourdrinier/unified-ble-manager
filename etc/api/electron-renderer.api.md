# API Report — unified-ble-manager/electron/renderer

```ts
export interface ElectronRendererBleManagerEnvironment { ... }
export interface ElectronDiscoveryDescriptor { ... }
export class ElectronRendererBleClient { ... }
export function createElectronRendererBleManager(...): Promise<BleManager>
export const createElectronRendererBleManagerWithEnvironment: typeof createElectronRendererBleManager
export function assertElectronAdvertisementObservation(...): void
export function isElectronConnectionEventsStreamHandle(handle: string): boolean
```
