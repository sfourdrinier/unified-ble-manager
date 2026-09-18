// ios/UnifiedBleRustCoreAdapterState.swift
//
// R02-Apple production adapter-state reader behind `adapter.state`:
// answers from the live CoreBluetooth central state. Mirrors Android's
// `RustCoreAdapterStateReader` (same record shape, same owned-radio vocab,
// same constant-minter `backendGeneration`). Thin read only — no policy,
// no caching, no session state.

import CoreBluetooth
import Foundation

/// Production `adapter.state` minter for the RustCore session facade.
/// Reuses the owned radio's snapshot projection so both surfaces report
/// one platform truth; adds the router-owned envelope fields
/// (`backendGeneration`, `updatedAt`) the provider shape-checks.
enum UnifiedBleRustCoreAdapterState {
  static func readAdapterStateJson() -> String {
    let probe = StateProbe.shared
    let snapshot = OwnedCoreBluetoothProtocolRadioSupport.adapterSnapshotDictionary(central: probe.central)
    var record: [String: Any] = [
      "availability": snapshot["availability"] as? String ?? "unknown",
      "authorization": snapshot["authorization"] as? String ?? "unknown",
      "power": snapshot["power"] as? String ?? "unknown",
      // The TS provider mints backendGeneration itself and ignores this
      // field (shape-checked only): a constant identifying the minter.
      "backendGeneration": "apple-router",
      "updatedAt": Int(Date().timeIntervalSince1970 * 1000),
    ]
    record["safeReason"] = snapshot["safeReason"] as? String ?? NSNull()
    guard let data = try? JSONSerialization.data(withJSONObject: record, options: [.sortedKeys]),
      let json = String(data: data, encoding: .utf8)
    else {
      // Unreachable with controlled values; stay honest if it ever fires.
      return "{\"availability\":\"unknown\",\"authorization\":\"unknown\",\"power\":\"unknown\","
        + "\"backendGeneration\":\"apple-router\",\"updatedAt\":0,"
        + "\"safeReason\":\"adapter snapshot serialization failed\"}"
    }
    return json
  }

  /// Dedicated central for state reads only. Never the owned radio's
  /// instance (the facade must not disturb radio ownership); created once
  /// so `.state` settles after the first report instead of re-arming
  /// `.unknown` on every read. State is polled synchronously — the
  /// delegate stays empty on purpose.
  private final class StateProbe: NSObject, CBCentralManagerDelegate {
    static let shared = StateProbe()

    let central: CBCentralManager

    private override init() {
      let queue = DispatchQueue(label: "com.ubm.rustcore.adapter-state")
      central = CBCentralManager(delegate: nil, queue: queue)
      super.init()
      central.delegate = self
    }

    func centralManagerDidUpdateState(_ central: CBCentralManager) {}
  }
}
