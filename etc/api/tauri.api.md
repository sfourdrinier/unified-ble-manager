# API Report — unified-ble-manager/tauri

```ts
export function createTauriBleManager(options?: BleManagerCreateOptions): Promise<BleManager>
export function createTauriBleManagerWithEnvironment(environment: TauriBleManagerEnvironment, options?: BleManagerCreateOptions): Promise<BleManager>
export interface TauriBleProvider { createManager(): Promise<BleManager> }
```
