// android/src/main/java/com/sfourdrinier/unifiedblemanager/rustcore/RustCoreAdapterStateReader.java

package com.sfourdrinier.unifiedblemanager.rustcore;

import android.Manifest;
import android.bluetooth.BluetoothAdapter;
import android.bluetooth.BluetoothManager;
import android.content.Context;
import android.content.pm.PackageManager;
import android.os.Build;

/**
 * Production {@link RustCoreSessionRouter.AdapterStateReader}: answers {@code adapter.state} from
 * the live platform Bluetooth adapter. Mapping mirrors the owned radio's
 * {@code currentProtocolAdapterState} (same availability/authorization/power vocab, same
 * safeReason texts) so both surfaces report one platform truth. Thin read only — no policy, no
 * caching, no session state. Hand-rolled JSON (no {@code org.json}, like the router): every
 * interpolated value is a fixed-vocab literal or a controlled safeReason without quotes.
 */
public final class RustCoreAdapterStateReader implements RustCoreSessionRouter.AdapterStateReader {

  private final Context appContext;

  public RustCoreAdapterStateReader(Context context) {
    this.appContext = context.getApplicationContext();
  }

  @Override
  public String readAdapterStateJson() {
    BluetoothManager managers = (BluetoothManager) appContext.getSystemService(Context.BLUETOOTH_SERVICE);
    BluetoothAdapter adapter = managers != null ? managers.getAdapter() : null;
    if (adapter == null) {
      return record(
          "unsupported",
          "unavailable",
          "unsupported",
          "This device does not expose an Android Bluetooth adapter.");
    }
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S
        && (appContext.checkSelfPermission(Manifest.permission.BLUETOOTH_SCAN)
                != PackageManager.PERMISSION_GRANTED
            || appContext.checkSelfPermission(Manifest.permission.BLUETOOTH_CONNECT)
                != PackageManager.PERMISSION_GRANTED)) {
      return record(
          "available", "denied", "unknown", "Bluetooth scan and connect permissions are not granted.");
    }
    final int state;
    try {
      state = adapter.getState();
    } catch (SecurityException error) {
      return record(
          "available", "denied", "unknown", "Android denied access to the Bluetooth adapter state.");
    }
    switch (state) {
      case BluetoothAdapter.STATE_ON:
        return record("available", "granted", "on", null);
      case BluetoothAdapter.STATE_OFF:
        return record("available", "granted", "off", null);
      case BluetoothAdapter.STATE_TURNING_ON:
      case BluetoothAdapter.STATE_TURNING_OFF:
        return record("available", "granted", "resetting", null);
      default:
        return record(
            "available",
            "granted",
            "unknown",
            "Android reported an unrecognized Bluetooth adapter state.");
    }
  }

  private static String record(String availability, String authorization, String power, String safeReason) {
    StringBuilder json = new StringBuilder(256);
    json.append("{\"availability\":\"")
        .append(availability)
        .append("\",\"authorization\":\"")
        .append(authorization)
        .append("\",\"power\":\"")
        .append(power)
        // The TS provider mints backendGeneration itself and ignores this
        // field (shape-checked only): a constant identifying the minter.
        .append("\",\"backendGeneration\":\"android-router\",\"updatedAt\":")
        .append(System.currentTimeMillis())
        .append(",\"safeReason\":");
    if (safeReason == null) {
      json.append("null");
    } else {
      json.append('"').append(safeReason).append('"');
    }
    json.append('}');
    return json.toString();
  }
}
