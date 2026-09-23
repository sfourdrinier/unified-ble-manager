<!-- example/README.md -->

# Classic React Native fixture

This is a repository fixture for the 4.0 React Native host. It constructs the
manager through `createReactNativeBleManager` with bytes, `AbortSignal`, and
snapshot-derived objects. Restoration identity is native-owned by the planned
native restoration integration; the fixture does not author a host restoration scope. Its dependency is
`unified-ble-manager: file:..`, so it exercises this checkout; it is not a
published-package install recipe.

Use a React Native 0.86+ native project with Android min SDK 24 or iOS deployment
target 16.4. From the repository root, install the fixture and its iOS pods when
needed:

```sh
pnpm --dir example install --no-frozen-lockfile
pnpm --dir example pods
```

Run the selected native host:

```sh
pnpm --dir example android
pnpm --dir example ios
```

Every one of those three scripts first checks the Metro port (8081 by default,
`RCT_METRO_PORT` to move it). If another project already serves that port, the
run stops with the holder's pid, working directory and command instead of
starting: a React Native app pointed at a foreign dev server loads that
project's bundle, registers no callable JavaScript modules, and then throws on
every native call without bound — an error loop that reads as a hang and eats
memory until the machine runs out. Where the guard cannot identify the holder
(no `lsof`) it says so and lets the run continue rather than guessing.

The fixture can exercise manager construction, scan, connection, GATT, and
cleanup against the device/permissions available to the app. It does not confer
hardware support for any Android or Apple environment. A simulator, compilation,
or local fixture run is not physical-radio, background, restoration, or
reliability evidence.

For a consuming application, install
`unified-ble-manager@5.0.0-rc.6`, after that exact version is published and read back
from npm, configure native permissions and lifecycle
ownership in that application, and follow the root [README](../README.md) and
[Expo plugin reference](../docs/EXPO_PLUGIN.md). The 4.0 package is Experimental;
do not use this fixture to infer Preview-or-higher support.
