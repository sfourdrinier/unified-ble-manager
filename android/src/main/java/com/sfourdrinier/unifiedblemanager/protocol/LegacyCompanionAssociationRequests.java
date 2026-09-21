// android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/LegacyCompanionAssociationRequests.java

package com.sfourdrinier.unifiedblemanager.protocol;

import android.bluetooth.le.ScanFilter;
import android.companion.AssociationRequest;
import android.companion.BluetoothLeDeviceFilter;
import android.os.ParcelUuid;

import java.util.regex.Pattern;

/**
 * Companion Device Manager request construction for the legacy protocol-control
 * route (`UnifiedBleProtocolControlModule.associateCompanionDevice`).
 *
 * <p>Kept in a dependency-free holder (no native library load) so plain JVM
 * unit tests can pin it. Test seam (finding 222 twin), pinned by
 * `UnifiedBleProtocolControlModuleAssociationTest`.
 *
 * <p>Finding 222 twin: association targets BLE peripherals, so the filter is
 * `BluetoothLeDeviceFilter` (API 26+; association itself requires API 33+, so
 * the existing TIRAMISU gate already covers the filter floor). The classic
 * `BluetoothDeviceFilter` only ever matched Bluetooth Classic peers, which is
 * why a BLE peripheral never appeared. `setSingleDevice` is only set when a
 * name scopes the request: unscoped + single-device offers an arbitrary
 * device, which is how the wrong association happened. This library is
 * BLE-only, so there is deliberately no classic/dual transport option.
 */
final class LegacyCompanionAssociationRequests {
  private LegacyCompanionAssociationRequests() {}

  static AssociationRequest build(String name, String serviceUuid) {
    final BluetoothLeDeviceFilter.Builder filter = new BluetoothLeDeviceFilter.Builder();
    if (name != null) filter.setNamePattern(Pattern.compile(Pattern.quote(name)));
    if (serviceUuid != null) {
      filter.setScanFilter(
          new ScanFilter.Builder()
              .setServiceUuid(ParcelUuid.fromString(normalizeUuid(serviceUuid)))
              .build());
    }
    return new AssociationRequest.Builder()
        .addDeviceFilter(filter.build())
        .setSingleDevice(name != null)
        .build();
  }

  private static String normalizeUuid(String value) {
    if (value.matches("[0-9A-Fa-f]{4}")) return "0000" + value + "-0000-1000-8000-00805F9B34FB";
    if (value.matches("[0-9A-Fa-f]{8}")) return value + "-0000-1000-8000-00805F9B34FB";
    if (value.matches("[0-9A-Fa-f]{8}(-[0-9A-Fa-f]{4}){3}-[0-9A-Fa-f]{12}")) return value;
    throw new IllegalArgumentException("Association serviceUuid must be a valid Bluetooth UUID");
  }
}
