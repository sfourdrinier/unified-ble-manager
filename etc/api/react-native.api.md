# API Report — unified-ble-manager/react-native

```ts
export function createReactNativeBleManager(options?: BleManagerCreateOptions): Promise<BleManager>
export function createReactNativeBleManagerWithEnvironment(options: ReactNativeBleManagerOptions): Promise<BleManager>
export function getNativeUnifiedBleProtocolControl(): NativeUnifiedBleProtocolControl
export type BleManagerCreateOptions = {
  readonly instanceId?: string
  readonly adapterId?: string
  readonly diagnostics?: DiagnosticsOptions
  readonly restoration?: {
    readonly applicationId: string
    readonly restorationId: string
    readonly generation?: string
  }
}
```

`createReactNativeBleManager()` is the application factory and does not accept
caller-authored `clientId`, `managerId`, or `hostSessionScope`. The explicit
`createReactNativeBleManagerWithEnvironment()` entrypoint is an injected
host/test seam; its internal options remain separate from the application API.
