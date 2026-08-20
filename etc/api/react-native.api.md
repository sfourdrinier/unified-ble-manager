# API Report — unified-ble-manager/react-native

```ts
export function createReactNativeBleManager(options?: BleManagerCreateOptions): Promise<BleManager>
export function createReactNativeBleManagerWithEnvironment(options: ReactNativeBleManagerOptions): Promise<BleManager>
export type ReactNativeBleManagerOptions = { platform: 'android' | 'apple', control: NativeUnifiedBleProtocolControl, now: () => number, clientId: string, managerId: string, hostSessionScope: string }
export type BleManagerCreateOptions = { instanceId?: string, adapterId?: string, diagnostics?: DiagnosticsOptions, restoration?: { applicationId: string, restorationId: string, generation?: string } }
```
