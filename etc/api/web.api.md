# API Report — unified-ble-manager/web

```ts
export function createWebBleManager(options?: BleManagerCreateOptions): Promise<BleManager>
export function createWebBleManagerWithEnvironment(options: WebBleManagerWithEnvironmentOptions): Promise<BleManager>
export function createNavigatorWebBluetoothProvider(environment: NavigatorWebBluetoothEnvironment): WebBluetoothProvider
export function createWebBluetoothProvider(boundary: WebBluetoothBoundary): WebBluetoothProvider
export interface WebBleManagerWithEnvironmentOptions { readonly environment: NavigatorWebBluetoothEnvironment; readonly createOptions?: BleManagerCreateOptions }
```
