# Migration build from version 1.1.0 to 2.0.0

> **Status: Historical record.** This migration note belongs to the
> `react-native-ble-plx` 1.x→2.x lineage and does not apply to
> `unified-ble-manager` 4.x. See the [documentation map](README.md).

1) Open `./ios/Podfile` file and remove following line:
   ```ruby
   pod 'react-native-ble-plx-swift', :path => '../node_modules/react-native-ble-plx'
   ```