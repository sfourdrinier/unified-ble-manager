<!-- example-web/README.md -->

# 4.0 TypeScript Web Bluetooth example

This Vite example constructs the host with `createWebBleManager()` from
`unified-ble-manager/web`. Its strict TypeScript source exercises the real Web
Bluetooth chooser and the shared 4.0 manager path:

It is the 4.0 clean-baseline Web Bluetooth example, now compiled rather than
maintained as an untyped script.

`choose / authorized → bounded connect → discover → battery read → heart-rate notify → disconnect → reconnect → destroy`

It has no React Native dependency, compatibility adapter, Base64 path, static
capability matrix, or product-specific transport contract. Every radio operation
has a deadline and every public error is displayed with its code, domain,
operation, and browser cause.

## Run

Use a current Chrome or Web Bluetooth-capable Chromium installation on a secure
context with a physical Heart Rate Service peripheral:

```bash
pnpm example:web
```

Open [http://localhost:5173](http://localhost:5173), then:

1. Select **Choose and connect** and choose the physical peripheral.
2. Confirm that discovery, Battery Level read, and Heart Rate notifications succeed.
3. Select **Disconnect**, then **Reconnect** to prove a fresh connection generation.
4. Select **Use authorized device** after a reload to exercise the origin-scoped peer directory.
5. Select **Destroy manager** and confirm that the displayed resource counters are zero.

Chrome cannot cancel a native `BluetoothRemoteGATTServer.connect()` promise. If
that operation reaches its UBM deadline, the example destroys the manager before
allowing a fresh retry. It never starts a second connection against retained
pending ownership.

The example is a live validation harness, but running it does not itself create a
release evidence receipt. A public support claim additionally requires retained,
checksum-bound logs and the exact packed artifact, browser, OS, adapter, peripheral,
and source identity required by `evidence/v1/`.
