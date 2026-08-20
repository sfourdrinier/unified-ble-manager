# API Report — unified-ble-manager/web

```ts
export function createWebBleManager(options?: BleManagerCreateOptions): Promise<BleManager>
export function createWebBleManagerWithEnvironment(options: WebBleManagerWithEnvironmentOptions): Promise<BleManager>
// Legacy tuple (kept for test compat, will move to /advanced)
export function createNavigatorWebBleManager(options: NavigatorWebBleManagerOptions): Promise<WebBleManagerSession>
export function createNavigatorWebBluetoothProvider(environment: NavigatorWebBluetoothEnvironment): WebBluetoothProvider
```
