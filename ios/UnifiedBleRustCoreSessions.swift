// ios/UnifiedBleRustCoreSessions.swift
//
// Pass-through behind the `UnifiedBleRustCore` TurboModule
// (src/NativeUnifiedBleRustCore.ts, docs/MOBILE_RUST_WIRE.md). It installs
// the one process host (`mobileHostInstall` over `UnifiedBleRustRadioAdapter`
// on the process-owned CoreBluetooth radio), holds the UniFFI session handles
// the module opened, routes Rust wakes to the module that owns the session,
// and hands Rust's JSON back verbatim. It keeps no op table, no revisions of
// its own and no tombstones: Rust mints session ids monotonically, so an id at
// or below the highest one issued that is no longer held is a closed session.
//
// Every refusal is the wire failure JSON `{"code","domain","operation","detail"}`
// the JS side parses strictly.

import Foundation
import CryptoKit
import Security

@objc(UnifiedBleRustCoreSessions)
@objcMembers
public final class UnifiedBleRustCoreSessions: NSObject, MobileWakeSink, @unchecked Sendable {
  public static let shared = UnifiedBleRustCoreSessions(installer: { try installProductionHost(wake: $0) })
  private static let launchLock = NSLock()
  private static var restorationLaunchIdentifiers = [String]()

  public func configureTerminalEvidenceMaximumBytes(_ maximumBytes: String) throws {
    guard let value = UInt64(maximumBytes), value > 0 else {
      throw MobileCoreError.Failed(code: "argument.invalid", domain: "core", operation: "mobile.terminal-evidence.configure", detail: "maximum must be a positive decimal integer")
    }
    // Rust admits a same-bound replay and rejects a conflict. This lets a
    // later manager verify the fixed process configuration without mutating
    // a host that has already installed its ledger.
    try mobileConfigureTerminalEvidenceMaximumBytes(maximumBytes: value)
  }
  private var accessoryStartupFailure: String?
  private var accessoryStartupQueryInFlight = false

  public static func recordNativeRestorationLaunchIdentifiers(_ identifiers: [String]) {
    launchLock.lock()
    restorationLaunchIdentifiers = identifiers
    launchLock.unlock()
  }

  private static func nativeRestorationLaunchIdentifiers() -> [String] {
    launchLock.lock()
    defer { launchLock.unlock() }
    return restorationLaunchIdentifiers
  }

  typealias Installer = (MobileWakeSink) throws -> MobileCoreHost

  private final class ProductionHostBinding: MobileHostBinding {
    let adapter: UnifiedBleRustRadioAdapter
    init(_ adapter: UnifiedBleRustRadioAdapter) { self.adapter = adapter }
    func bind(host: MobileCoreHost) -> Bool {
      guard let authority = host.claimNotificationFailureSpoolConfigurationAuthorityV3() else { return false }
      guard let scanRecoveryAuthority = host.claimScanFailureRecoveryConfigurationAuthorityV1() else { return false }
      // Claim before attaching the live sink. A rejected/duplicate authority
      // must not leave a host callback route that another binding can drive.
      guard adapter.installNotificationFailureSpoolConfigurationAuthority(authority) else { return false }
      // This authority remains inside native code. It accepts only a
      // host-authorized opaque recording identifier and is never bridged to JS.
      guard adapter.installScanFailureRecoveryConfigurationAuthority(scanRecoveryAuthority) else { return false }
      guard adapter.bind(sink: host) else { return false }
      adapter.installNotificationFailureSpool(host)
      return true
    }
  }

  private final class ProductionInstallDebt {
    let radio: OwnedCoreBluetoothProtocolRadio
    let adapter: UnifiedBleRustRadioAdapter
    init(radio: OwnedCoreBluetoothProtocolRadio, adapter: UnifiedBleRustRadioAdapter) {
      self.radio = radio
      self.adapter = adapter
    }
  }
  private static let productionDebtLock = NSLock()
  private static var productionDebt: ProductionInstallDebt?
  private static var productionInstallInProgress = false

  private static func retryProductionInstallDebt() {
    productionDebtLock.lock()
    let debt = productionDebt
    productionDebtLock.unlock()
    guard let debt else { return }
    if mobileHostHasPendingCleanup() {
      let result = mobileHostRetryPendingCleanup()
      guard !mobileHostHasPendingCleanup() else {
        NSLog("[UnifiedBleRustCoreSessions] retained host cleanup is pending: %@", result)
        return
      }
    }
    debt.radio.releaseBorrowerIfCurrent(debt.adapter) { error in
      if let error {
        NSLog("[UnifiedBleRustCoreSessions] retrying retained borrower release: %@", error)
        DispatchQueue.global().asyncAfter(deadline: .now() + .seconds(1)) {
          retryProductionInstallDebt()
        }
        return
      }
      productionDebtLock.lock()
      if productionDebt === debt { productionDebt = nil }
      productionDebtLock.unlock()
    }
  }

  private struct Entry {
    let session: MobileCoreSession
    weak var owner: NSObject?
    let ownerState: OwnerState
    let onWake: (String) -> Void
  }

  private final class OwnerState {
    weak var owner: NSObject?
    var invalidated = false
    var opening = 0
    init(_ owner: NSObject) { self.owner = owner }
  }

  typealias OpenInvoker = (MobileCoreHost, String, String) throws -> MobileCoreSession
  typealias DisposeInvoker = (MobileCoreSession, @escaping (String) -> Void) -> Void
  private struct DisposalFlight {
    let generation: UInt64
    var completions: [(String?) -> Void]
  }

  private let installer: Installer
  private let openInvoker: OpenInvoker
  private let disposeInvoker: DisposeInvoker
  private let retryDelay: DispatchTimeInterval
  private let lock = NSLock()
  private let continuationDeclarationGate = NSRecursiveLock()
  private let hostLock = NSLock()
  private var host: MobileCoreHost?
  private var hostInstalling = false
  // The production adapter is the sole holder of the spool-configuration
  // authority. Keep it paired with the exact host it bound, rather than
  // recovering an adapter by process-global radio state.
  private var notificationFailureSpoolAdapter: (host: MobileCoreHost, adapter: UnifiedBleRustRadioAdapter)?
  // Scan-failure recovery uses the same process-host boundary. Its selected
  // recording id is native embedding state, never an ObjC/TurboModule input.
  private var scanFailureRecoveryAdapter: (host: MobileCoreHost, adapter: UnifiedBleRustRadioAdapter)?
  private let scanFailureRecoverySelectionLock = NSLock()
  private var selectedScanFailureRecoveryRecordingId: String?
  private var scanFailureRecoverySelectionGeneration: UInt64 = 0
  // Swift-internal deterministic fixture hooks. They are deliberately absent
  // from ObjC/TurboModule and are installed only before a harness exercises a
  // production recovery path.
  private var testingBeforeSelectedScanRecoveryOpen: (() -> Void)?
  private var testingBeforeScanRecoveryCleanupRetire: (() throws -> Void)?
  private let scanFailureRecoveryHandlesLock = NSLock()
  private struct ScanFailureRecoveryHandles {
    let generation: UInt64
    let lease: UnifiedBleRustScanFailureRecoveryLease
    var owners: [String: MobileScanFailureRecoveryOwner] = [:]
    var ownerHandles: [String: String] = [:]
    var heads: [String: MobileScanFailureRecoveryHead] = [:]
    var headHandles: [String: String] = [:]
    var cleanup: [String: MobileScanFailureCleanupReceipt] = [:]
    var cleanupEnumerated = false
  }
  private var scanFailureRecoveryHandles: [String: ScanFailureRecoveryHandles] = [:]
  private static let maxScanRecoveryAliases = 1024

  /// The only binary value returned to the recovery JSI host object. Keeping
  /// metadata and bytes together makes one Rust read the atomic boundary.
  @objc(UnifiedBleRustScanFailureRecoveryPage)
  public final class ScanFailureRecoveryPage: NSObject {
    @objc public let offset: String
    @objc public let nextOffset: String
    @objc public let done: Bool
    @objc public let bytes: NSData
    init(offset: String, nextOffset: String, done: Bool, bytes: Data) {
      self.offset = offset
      self.nextOffset = nextOffset
      self.done = done
      self.bytes = bytes as NSData
    }
  }
  @objc(UnifiedBleRustScanFailureRawToken)
  public final class ScanFailureRawToken: NSObject {
    @objc public let kind: String; @objc public let value: NSObject?; @objc public let count: String?; @objc public let total: String?; @objc public let bytes: NSData?
    init(kind: String, value: NSObject? = nil, count: String? = nil, total: String? = nil, bytes: Data? = nil) { self.kind = kind; self.value = value; self.count = count; self.total = total; self.bytes = bytes as NSData? }
  }
  private let recordingQueue: DispatchQueue
  private let invokeQueue = DispatchQueue(label: "com.ubm.rust-core.invoke")
  private let recordingLock = NSLock()
  private var recordingConfigured = false
  // Internal fixture seam only: production always resolves Application Support.
  // It is deliberately Swift-only (not exported through ObjC/TurboModule) so an
  // embedding app cannot redirect durable recording storage at runtime.
  private var testingRecordingStorageDirectory: URL?

  private struct RecordingFailure: Error {
    let json: String
    var envelope: String? = nil
  }

  private func configureRecordingStorage() throws {
    recordingLock.lock()
    defer { recordingLock.unlock() }
    if recordingConfigured { return }
    do {
    let manager = FileManager.default
    var directory: URL
    if let testingRecordingStorageDirectory {
      directory = testingRecordingStorageDirectory
    } else {
      directory = try manager.url(for: .applicationSupportDirectory, in: .userDomainMask,
        appropriateFor: nil, create: true).appendingPathComponent("ubm-continuation", isDirectory: true)
    }
    try manager.createDirectory(at: directory, withIntermediateDirectories: true)
    var attributes = URLResourceValues()
    attributes.isExcludedFromBackup = true
    try directory.setResourceValues(attributes)
    #if os(iOS) || os(tvOS)
    try manager.setAttributes([.protectionKey: FileProtectionType.completeUntilFirstUserAuthentication], ofItemAtPath: directory.path)
    #endif
    let envelope = mobileRecordingConfigureDirectory(path: directory.path)
    guard let data = envelope.data(using: .utf8),
          let result = try JSONSerialization.jsonObject(with: data) as? [String: Any] else {
      throw MobileCoreError.Failed(code: "protocol.malformed", domain: "protocol", operation: "continuation.recording.configure", detail: "Malformed storage configuration response")
    }
    if result["ok"] as? Bool != true {
      guard let error = result["error"] as? [String: Any], error["code"] is String,
            error["domain"] is String, error["operation"] is String else {
        throw MobileCoreError.Failed(code: "protocol.malformed", domain: "protocol", operation: "continuation.recording.configure", detail: "Malformed storage configuration failure")
      }
      throw RecordingFailure(json: String(decoding: try JSONSerialization.data(withJSONObject: error, options: [.sortedKeys]), as: UTF8.self), envelope: envelope)
    }
    recordingConfigured = true
    } catch let failure as RecordingFailure { throw failure }
      catch { throw RecordingFailure(json: Self.recordingFailureJson(error)) }
  }

  /// Installs an isolated storage root before this process configures durable
  /// recording. This is intentionally internal to the Swift module so the
  /// Apple native harness can exercise the production configuration path
  /// without relying on Foundation's user-home lookup.
  func installTestingRecordingStorageDirectory(_ directory: URL) throws {
    recordingLock.lock()
    defer { recordingLock.unlock() }
    guard !recordingConfigured else {
      throw MobileCoreError.Failed(code: "lifecycle.invalid-state", domain: "restoration", operation: "continuation.recording.configure", detail: "recording storage is already configured")
    }
    testingRecordingStorageDirectory = directory
  }

  /// Installs deterministic, Swift-only lifecycle barriers for the Apple
  /// harness. Passing `nil` clears an existing hook. Production embeddings
  /// cannot access this surface through Objective-C or React Native.
  func installTestingScanRecoveryHooks(beforeOpen: (() -> Void)? = nil,
                                       beforeCleanupRetire: (() throws -> Void)? = nil) {
    scanFailureRecoverySelectionLock.lock()
    testingBeforeSelectedScanRecoveryOpen = beforeOpen
    scanFailureRecoverySelectionLock.unlock()
    scanFailureRecoveryHandlesLock.lock()
    testingBeforeScanRecoveryCleanupRetire = beforeCleanupRetire
    scanFailureRecoveryHandlesLock.unlock()
  }

  /// Probe only the linked native binary; this does not open storage or radio.
  public func orderedRecordingVersion() -> UInt32 { mobileOrderedRecordingVersion() }

  private func ownedEntry(_ sessionId: String, owner: NSObject, operation: String) -> Addressed {
    guard let id = Self.sessionNumber(sessionId) else {
      return .failure(Self.failureJson(code: "argument.invalid", domain: "core", operation: operation, detail: "invalid session id"))
    }
    lock.lock()
    let found = sessions[id]
    let authorized = found?.owner === owner && found?.ownerState.invalidated == false
    lock.unlock()
    guard authorized, let found else {
      return .failure(Self.failureJson(code: "ownership.denied", domain: "core", operation: operation, detail: "session is not owned by this module"))
    }
    return .success(found)
  }

  public func negotiateScanEndVersion(sessionId: String, offeredJson: String, ownerToken: NSObject,
                                      completion: (String?, String?) -> Void) {
    let operation = "scan-end-version"
    switch ownedEntry(sessionId, owner: ownerToken, operation: operation) {
    case let .failure(failure): completion(nil, failure)
    case let .success(entry):
      do { completion(try entry.session.negotiateScanEndVersionQualified(offeredJson: offeredJson), nil) }
      catch { completion(nil, Self.failureJson(error, operation: operation)) }
    }
  }

  public func orderedRecordingReceiptControl(sessionId: String, id: String, operation: String, ownerToken: NSObject,
                                             completion: @escaping (String?, String?) -> Void) {
    guard operation == "reserve" || operation == "status" || operation == "retire" else {
      return completion(nil, Self.failureJson(code: "argument.invalid", domain: "core", operation: "ordered-recording.receipt", detail: "invalid operation"))
    }
    recordingQueue.async {
      switch self.ownedEntry(sessionId, owner: ownerToken, operation: "ordered-recording.receipt") {
      case let .failure(failure): completion(nil, failure)
      case let .success(entry): entry.session.orderedRecordingReceiptControl(operation: operation, id: id, completion: InvokeCompletion { completion($0, nil) })
      }
    }
  }

  public func orderedRecordingStart(sessionId: String, handle: String, id: String, maxBytes: String, maxRecords: String,
                                    declarationJson: String, budgetMs: Double, ownerToken: NSObject,
                                    completion: @escaping (String?, String?) -> Void) {
    guard let budget = UInt64(exactly: budgetMs), budget <= UInt64(Int32.max) else {
      return completion(nil, Self.failureJson(code: "argument.invalid", domain: "core", operation: "ordered-recording.start", detail: "invalid budget"))
    }
    recordingQueue.async {
      guard case .success = self.ownedEntry(sessionId, owner: ownerToken, operation: "ordered-recording.start") else {
        return completion(nil, Self.failureJson(code: "ownership.denied", domain: "core", operation: "ordered-recording.start", detail: "session is not owned by this module"))
      }
      do {
        try self.configureRecordingStorage()
        switch self.ownedEntry(sessionId, owner: ownerToken, operation: "ordered-recording.start") {
        case let .failure(failure): completion(nil, failure)
        case let .success(entry):
          entry.session.startOrderedRecording(handle: handle, recordingId: id, maxBytes: maxBytes, maxRecords: maxRecords,
            declarationJson: declarationJson, budgetMs: budget, completion: InvokeCompletion { completion($0, nil) })
        }
      } catch { completion(nil, Self.recordingFailureJson(error)) }
    }
  }

  public func orderedRecordingControl(sessionId: String, id: String, stop: Bool, ownerToken: NSObject,
                                      completion: @escaping (String?, String?) -> Void) {
    recordingQueue.async {
      guard case .success = self.ownedEntry(sessionId, owner: ownerToken, operation: "ordered-recording.control") else {
        return completion(nil, Self.failureJson(code: "ownership.denied", domain: "core", operation: "ordered-recording.control", detail: "session is not owned by this module"))
      }
      do {
        try self.configureRecordingStorage()
        switch self.ownedEntry(sessionId, owner: ownerToken, operation: "ordered-recording.control") {
        case let .failure(failure): completion(nil, failure)
        case let .success(entry):
          let callback = InvokeCompletion { completion($0, nil) }
          if stop { entry.session.stopOrderedRecording(id: id, completion: callback) }
          else { entry.session.orderedRecordingContext(id: id, completion: callback) }
        }
      } catch { completion(nil, Self.recordingFailureJson(error)) }
    }
  }

  public func orderedRecordingOffline(operation: String, id: String, completion: @escaping (String?, String?) -> Void) {
    guard operation == "context" || operation == "status" || operation == "stop" else {
      return completion(nil, Self.failureJson(code: "argument.invalid", domain: "core", operation: "ordered-recording.offline", detail: "invalid operation"))
    }
    recordingQueue.async {
      do {
        try self.configureRecordingStorage()
        mobileRecordingOrderedOffline(operation: operation, id: id, completion: InvokeCompletion { completion($0, nil) })
      } catch { completion(nil, Self.recordingFailureJson(error)) }
    }
  }

  public func recordingControl(_ operation: String, id: String, token: String, maxItems: Double, maxBytes: Double,
    completion: @escaping (String?, String?) -> Void) {
    recordingQueue.async {
      do {
        guard maxItems.isFinite, maxBytes.isFinite, maxItems >= 0, maxBytes >= 0,
              maxItems <= Double(UInt32.max), maxBytes <= Double(UInt32.max),
              maxItems.rounded(.towardZero) == maxItems, maxBytes.rounded(.towardZero) == maxBytes else {
          throw MobileCoreError.Failed(code: "argument.invalid", domain: "restoration", operation: "continuation.recording", detail: "invalid drain bounds")
        }
        try self.configureRecordingStorage()
        completion(mobileRecordingControl(operation: operation, id: id, token: token,
          maxItems: UInt32(maxItems), maxBytes: UInt32(maxBytes)), nil)
      } catch { completion(nil, Self.recordingFailureJson(error)) }
    }
  }

  /// Explicitly attaches an existing host-owned recording to the durable
  /// notification-failure sidecar. This never creates a recording or accepts
  /// a path: Rust validates the opaque id against its live recording registry.
  public func notificationFailureSpoolConfigure(_ recordingId: String, maxBytes: String, maxRecords: String,
                                                completion: @escaping (String?, String?) -> Void) {
    recordingQueue.async {
      do {
        guard !recordingId.isEmpty, let quotaBytes = UInt64(maxBytes), let quotaRecords = UInt64(maxRecords),
              quotaBytes > 0, quotaRecords > 0 else {
          throw MobileCoreError.Failed(code: "argument.invalid", domain: "restoration",
            operation: "notification-failure-spool.configure", detail: "recording id or spool quota is invalid")
        }
        try self.configureRecordingStorage()
        let host = try self.installedHost()
        guard let adapter = self.notificationFailureSpoolAdapter(for: host) else {
          throw MobileCoreError.Failed(code: "lifecycle.invalid-state", domain: "core",
            operation: "notification-failure-spool.configure", detail: "installed host has no production spool adapter")
        }
        let change = adapter.reconfigureNotificationFailureSpoolV2(recordingId: recordingId,
          maxBytes: maxBytes, maxRecords: maxRecords)
        guard change.admitted, let result = change.result else {
          completion("{\"status\":\"error\",\"error\":{\"domain\":\"storage\",\"code\":\"storage.busy\"}}", nil)
          return
        }
        completion(result, nil)
      } catch { completion(nil, Self.recordingFailureJson(error)) }
    }
  }

  /// Native embedding bootstrap only. This method is intentionally Swift-only
  /// (no `@objc` export): React Native cannot select a sidecar, path, or
  /// recovery credential. Calling it again replaces the app-private selected
  /// recording before a recovery handle is opened.
  public func selectScanFailureRecoveryRecording(_ recordingId: String) throws {
    guard Self.isNativeRecordingName(recordingId) else {
      throw MobileCoreError.Failed(code: "argument.invalid", domain: "restoration",
        operation: "scan.failure-recovery.select", detail: "recording id is not a native recording name")
    }
    scanFailureRecoverySelectionLock.lock()
    selectedScanFailureRecoveryRecordingId = recordingId
    scanFailureRecoverySelectionGeneration &+= 1
    scanFailureRecoveryHandlesLock.lock()
    scanFailureRecoveryHandles.removeAll()
    scanFailureRecoveryHandlesLock.unlock()
    scanFailureRecoverySelectionLock.unlock()
  }

  /// Opens the one recording selected by the native embedding and returns
  /// opaque Swift foreign handles. Rust validates that the selected id names
  /// a live host-owned recording; this method never creates a recording.
  public func openSelectedScanFailureRecovery() throws -> UnifiedBleRustScanFailureRecoveryLease {
    scanFailureRecoverySelectionLock.lock()
    defer { scanFailureRecoverySelectionLock.unlock() }
    let recordingId = selectedScanFailureRecoveryRecordingId
    guard let recordingId else {
      throw MobileCoreError.Failed(code: "capability.unsupported", domain: "restoration",
        operation: "scan.failure-recovery.open", detail: "no native scan recovery recording is selected")
    }
    testingBeforeSelectedScanRecoveryOpen?()
    try configureRecordingStorage()
    let host = try installedHost()
    guard let adapter = scanFailureRecoveryAdapter(for: host) else {
      throw MobileCoreError.Failed(code: "lifecycle.invalid-state", domain: "core",
        operation: "scan.failure-recovery.open", detail: "installed host has no scan recovery adapter")
    }
    return try adapter.openScanFailureRecovery(recordingId: recordingId)
  }

  private func scanRecoveryJSON(_ value: Any) throws -> String {
    String(decoding: try JSONSerialization.data(withJSONObject: value, options: [.sortedKeys]), as: UTF8.self)
  }

  private func scanRecoveryHandle() -> String { UUID().uuidString.lowercased() }

  /// Terminal raw-owner controls invalidate only their own alias. Preserve all
  /// other active aliases even when inventories are repeated or reordered.
  private func reclaimOwnerAlias(_ handle: String, from state: inout ScanFailureRecoveryHandles) {
    state.owners.removeValue(forKey: handle)
    if let identity = state.ownerHandles.first(where: { $0.value == handle })?.key {
      state.ownerHandles.removeValue(forKey: identity)
    }
  }

  private func selectedScanRecoveryGeneration() -> UInt64 {
    scanFailureRecoverySelectionLock.lock(); defer { scanFailureRecoverySelectionLock.unlock() }
    return scanFailureRecoverySelectionGeneration
  }

  @objc public func hasSelectedScanFailureRecoveryRecording() -> Bool {
    scanFailureRecoverySelectionLock.lock(); defer { scanFailureRecoverySelectionLock.unlock() }
    return selectedScanFailureRecoveryRecordingId != nil
  }

  private func withScanRecovery<T>(_ leaseId: String, operation: String,
                                   _ body: (inout ScanFailureRecoveryHandles) throws -> T) throws -> T {
    scanFailureRecoverySelectionLock.lock()
    let generation = scanFailureRecoverySelectionGeneration
    scanFailureRecoveryHandlesLock.lock()
    defer { scanFailureRecoveryHandlesLock.unlock(); scanFailureRecoverySelectionLock.unlock() }
    guard var state = scanFailureRecoveryHandles[leaseId], state.generation == generation else {
      throw MobileCoreError.Failed(code: "ownership.denied", domain: "restoration", operation: operation,
        detail: "scan recovery handle is stale, foreign, or revoked")
    }
    try state.lease.requireLive(operation)
    let result = try body(&state)
    // A completion from a prior selection or binding must not be surfaced as
    // if it belonged to the current native recovery route.
    try state.lease.requireLive(operation)
    scanFailureRecoveryHandles[leaseId] = state
    return result
  }

  /// Opaque-handle bridge for the TurboModule/JSI pair. No recording id or
  /// Rust capability leaves this boundary; each handle is selection-scoped.
  public func scanFailureRecoveryOpen(completion: @escaping (String?, String?) -> Void) {
    recordingQueue.async {
      do {
        self.scanFailureRecoverySelectionLock.lock()
        defer { self.scanFailureRecoverySelectionLock.unlock() }
        guard let recordingId = self.selectedScanFailureRecoveryRecordingId else {
          throw MobileCoreError.Failed(code: "capability.unsupported", domain: "restoration", operation: "scan.failure-recovery.open", detail: "no native scan recovery recording is selected")
        }
        let generation = self.scanFailureRecoverySelectionGeneration
        try self.configureRecordingStorage()
        let host = try self.installedHost()
        guard let adapter = self.scanFailureRecoveryAdapter(for: host) else {
          throw MobileCoreError.Failed(code: "lifecycle.invalid-state", domain: "core", operation: "scan.failure-recovery.open", detail: "installed host has no scan recovery adapter")
        }
        let lease = try adapter.openScanFailureRecovery(recordingId: recordingId)
        let id = self.scanRecoveryHandle()
        self.scanFailureRecoveryHandlesLock.lock()
        self.scanFailureRecoveryHandles[id] = ScanFailureRecoveryHandles(generation: generation, lease: lease)
        self.scanFailureRecoveryHandlesLock.unlock()
        completion(try self.scanRecoveryJSON(["leaseId": id]), nil)
      } catch { completion(nil, Self.failureJson(error, operation: "scan.failure-recovery.open")) }
    }
  }

  // Process-selected terminal recovery has no caller-selected manager or
  // recording identifier. UniFFI keeps recovery aliases native-local.
  public func terminalEvidenceRecoveryOpenSelected() throws -> String {
    try installedHost().terminalEvidenceRecoveryOpenSelected()
  }
  public func terminalEvidenceRecoveryList(_ leaseId: String, maxItems: UInt32, maxBytes: UInt64) throws -> String {
    try installedHost().terminalEvidenceRecoveryList(leaseId: leaseId, maxItems: maxItems, maxBytes: maxBytes)
  }
  public func terminalEvidenceRecoveryListWithStatus(_ leaseId: String, maxItems: UInt32, maxBytes: UInt64) throws -> String {
    try installedHost().terminalEvidenceRecoveryListWithStatus(leaseId: leaseId, maxItems: maxItems, maxBytes: maxBytes)
  }
  public func terminalEvidenceRecoveryRead(_ handle: String, offset: String, maxBytes: UInt32) throws -> String {
    try installedHost().terminalEvidenceRecoveryRead(handle: handle, offset: offset, maxBytes: maxBytes)
  }
  public func terminalEvidenceRecoveryAcknowledge(_ handle: String) throws -> String {
    try installedHost().terminalEvidenceRecoveryAcknowledge(handle: handle)
  }

  public func scanFailureRecoveryOwners(_ leaseId: String, completion: @escaping (String?, String?) -> Void) {
    do {
      let value: [[String: Any]] = try withScanRecovery(leaseId, operation: "scan.failure-recovery.owners") { state in
        let owners = try state.lease.owners()
        // Obtain the durable owner identities before mutating the opaque-handle
        // registry. Repeated inventories must preserve live handles, while a
        // new inventory cannot partially consume the bounded alias capacity.
        let describedOwners = try owners.map { owner in (owner, try owner.info()) }
        let newOwners = describedOwners.reduce(into: 0) { count, described in
          if state.ownerHandles[described.1.ownerId] == nil { count += 1 }
        }
        guard state.owners.count <= Self.maxScanRecoveryAliases - newOwners else {
          throw MobileCoreError.Failed(code: "storage.busy", domain: "restoration", operation: "scan.failure-recovery.owners", detail: "owner alias capacity exceeded")
        }
        return describedOwners.map { owner, info in
          let id = state.ownerHandles[info.ownerId] ?? scanRecoveryHandle()
          state.ownerHandles[info.ownerId] = id; state.owners[id] = owner
          return ["handleId": id, "prepared": info.prepared, "producerFinished": info.producerFinished,
                  "rawRetired": info.rawRetired, "rawReleased": info.rawReleased,
                  "memberCount": NSNumber(value: info.memberCount), "released": info.released]
        }
      }
      completion(try scanRecoveryJSON(["owners": value]), nil)
    } catch { completion(nil, Self.failureJson(error, operation: "scan.failure-recovery.owners")) }
  }

  public func scanFailureRecoveryMember(_ leaseId: String, handleId: String, index: Double,
                                        completion: @escaping (String?, String?) -> Void) {
    do {
      guard index.isFinite, index >= 0, index.rounded(.towardZero) == index, index <= Double(UInt64.max) else {
        throw MobileCoreError.Failed(code: "argument.invalid", domain: "restoration", operation: "scan.failure-recovery.member", detail: "index is invalid")
      }
      let member: MobileScanFailureMember = try withScanRecovery(leaseId, operation: "scan.failure-recovery.member") { state in
        guard let owner = state.owners[handleId] else { throw MobileCoreError.Failed(code: "ownership.denied", domain: "restoration", operation: "scan.failure-recovery.member", detail: "owner handle is stale") }
        return try owner.member(index: UInt64(index))
      }
      completion(try scanRecoveryJSON(["member": ["recipient": member.recipient.map { String(format: "%02x", $0) }.joined(), "delivery": member.delivery.map { String(format: "%02x", $0) }.joined()]]), nil)
    } catch { completion(nil, Self.failureJson(error, operation: "scan.failure-recovery.member")) }
  }

  private func scanRecoveryLeaseId(ownerHandle: String, operation: String) throws -> String {
    scanFailureRecoverySelectionLock.lock()
    let generation = scanFailureRecoverySelectionGeneration
    scanFailureRecoveryHandlesLock.lock(); defer { scanFailureRecoveryHandlesLock.unlock(); scanFailureRecoverySelectionLock.unlock() }
    guard let pair = scanFailureRecoveryHandles.first(where: { $0.value.generation == generation && $0.value.owners[ownerHandle] != nil }) else {
      throw MobileCoreError.Failed(code: "ownership.denied", domain: "restoration", operation: operation, detail: "owner handle is stale")
    }
    return pair.key
  }

  public func scanFailureRecoveryMember(ownerHandle: String, index: Double, completion: @escaping (String?, String?) -> Void) {
    do { try scanFailureRecoveryMember(scanRecoveryLeaseId(ownerHandle: ownerHandle, operation: "scan.failure-recovery.member"), handleId: ownerHandle, index: index, completion: completion) }
    catch { completion(nil, Self.failureJson(error, operation: "scan.failure-recovery.member")) }
  }

  // Raw-source metadata/control mirrors the desktop opaque-owner route. Raw
  // token bytes deliberately await the shared JSI binary contract.
  public func scanFailureRecoveryRawInfo(ownerHandle: String, completion: @escaping (String?, String?) -> Void) {
    do {
      let lease = try scanRecoveryLeaseId(ownerHandle: ownerHandle, operation: "scan.failure-recovery.raw-info")
      let result: [String: Any] = try withScanRecovery(lease, operation: "scan.failure-recovery.raw-info") { state in
        guard let owner = state.owners[ownerHandle] else { throw MobileCoreError.Failed(code: "ownership.denied", domain: "restoration", operation: "scan.failure-recovery.raw-info", detail: "owner handle is stale") }
        let info = try owner.rawInfo()
        let hex = { (bytes: Data) in bytes.map { String(format: "%02x", $0) }.joined() }
        return ["identity": ["host": hex(info.identity.host), "scan": hex(info.identity.scan), "start": hex(info.identity.start), "sourceGeneration": hex(info.identity.sourceGeneration), "occurrence": hex(info.identity.occurrence)], "memberCount": NSNumber(value: info.memberCount), "tokenCount": NSNumber(value: info.tokenCount)]
      }
      completion(try scanRecoveryJSON(result), nil)
    } catch { completion(nil, Self.failureJson(error, operation: "scan.failure-recovery.raw-info")) }
  }

  public func scanFailureRecoveryRawMember(ownerHandle: String, index: Double, completion: @escaping (String?, String?) -> Void) {
    do {
      guard index.isFinite, index >= 0, index.rounded(.towardZero) == index, index <= Double(UInt64.max) else { throw MobileCoreError.Failed(code: "argument.invalid", domain: "restoration", operation: "scan.failure-recovery.raw-member", detail: "index is invalid") }
      let lease = try scanRecoveryLeaseId(ownerHandle: ownerHandle, operation: "scan.failure-recovery.raw-member")
      let member: MobileScanFailureMember = try withScanRecovery(lease, operation: "scan.failure-recovery.raw-member") { state in
        guard let owner = state.owners[ownerHandle] else { throw MobileCoreError.Failed(code: "ownership.denied", domain: "restoration", operation: "scan.failure-recovery.raw-member", detail: "owner handle is stale") }
        return try owner.rawMember(index: UInt64(index))
      }
      completion(try scanRecoveryJSON(["member": ["recipient": member.recipient.map { String(format: "%02x", $0) }.joined(), "delivery": member.delivery.map { String(format: "%02x", $0) }.joined()]]), nil)
    } catch { completion(nil, Self.failureJson(error, operation: "scan.failure-recovery.raw-member")) }
  }

  @objc(readReactNativeScanFailureRecoveryRawToken:tokenIndex:offset:maxBytes:error:)
  public func readReactNativeScanFailureRecoveryRawToken(_ ownerHandle: String, tokenIndex: Double, offset: String, maxBytes: String) throws -> ScanFailureRawToken {
    guard tokenIndex.isFinite, tokenIndex >= 0, tokenIndex.rounded(.towardZero) == tokenIndex, tokenIndex <= 9_007_199_254_740_991,
          offset.range(of: "^(?:0|[1-9][0-9]*)$", options: .regularExpression) != nil,
          maxBytes.range(of: "^(?:[1-9][0-9]*)$", options: .regularExpression) != nil,
          let boundedMaxBytes = UInt32(maxBytes), boundedMaxBytes <= 64 * 1024 else { throw MobileCoreError.Failed(code: "argument.invalid", domain: "restoration", operation: "scan.failure-recovery.raw-token", detail: "invalid bounded raw-token request") }
    let lease = try scanRecoveryLeaseId(ownerHandle: ownerHandle, operation: "scan.failure-recovery.raw-token")
    return try withScanRecovery(lease, operation: "scan.failure-recovery.raw-token") { state in
      guard let owner = state.owners[ownerHandle] else { throw MobileCoreError.Failed(code: "ownership.denied", domain: "restoration", operation: "scan.failure-recovery.raw-token", detail: "owner handle is stale") }
      switch try owner.rawToken(index: UInt64(tokenIndex), offset: offset, maxBytes: boundedMaxBytes) {
      case .null: return ScanFailureRawToken(kind: "null")
      case let .bool(value): return ScanFailureRawToken(kind: "bool", value: NSNumber(value: value))
      case let .i64(value): return ScanFailureRawToken(kind: "i64", value: String(value) as NSString)
      case let .u64(value): return ScanFailureRawToken(kind: "u64", value: value as NSString)
      case let .f64Bits(value): return ScanFailureRawToken(kind: "f64Bits", value: value as NSString)
      case let .map(entries): return ScanFailureRawToken(kind: "map", count: entries)
      case let .array(entries): return ScanFailureRawToken(kind: "array", count: entries)
      case let .text(total, bytes): return ScanFailureRawToken(kind: "text", total: total, bytes: bytes)
      case let .bytes(total, bytes): return ScanFailureRawToken(kind: "bytes", total: total, bytes: bytes)
      }
    }
  }

  public func scanFailureRecoveryRawControl(ownerHandle: String, operation: String, completion: @escaping (String?, String?) -> Void) {
    do {
      let lease = try scanRecoveryLeaseId(ownerHandle: ownerHandle, operation: "scan.failure-recovery.raw-control")
      let result: [String: Any] = try withScanRecovery(lease, operation: "scan.failure-recovery.raw-control") { state in
        guard let owner = state.owners[ownerHandle] else { throw MobileCoreError.Failed(code: "ownership.denied", domain: "restoration", operation: "scan.failure-recovery.raw-control", detail: "owner handle is stale") }
        switch operation {
        case "retire":
          try owner.rawRetire()
          self.reclaimOwnerAlias(ownerHandle, from: &state)
          return ["cleanupReceiptId": NSNull()]
        case "release":
          try owner.rawRelease()
          self.reclaimOwnerAlias(ownerHandle, from: &state)
          return ["cleanupReceiptId": NSNull()]
        case "finalize":
          guard state.cleanup.count < Self.maxScanRecoveryAliases else { throw MobileCoreError.Failed(code: "storage.busy", domain: "restoration", operation: "scan.failure-recovery.raw-control", detail: "cleanup alias capacity exceeded") }
          let receipt = try owner.rawFinalize()
          let id = scanRecoveryHandle()
          state.cleanup[id] = receipt
          self.reclaimOwnerAlias(ownerHandle, from: &state)
          return ["cleanupReceiptId": id]
        default: throw MobileCoreError.Failed(code: "argument.invalid", domain: "restoration", operation: "scan.failure-recovery.raw-control", detail: "unsupported control")
        }
      }
      completion(try scanRecoveryJSON(result), nil)
    } catch { completion(nil, Self.failureJson(error, operation: "scan.failure-recovery.raw-control")) }
  }

  public func scanFailureRecoveryHead(_ leaseId: String, handleId: String, index: Double,
                                      completion: @escaping (String?, String?) -> Void) {
    do {
      guard index.isFinite, index >= 0, index.rounded(.towardZero) == index, index <= Double(UInt64.max) else {
        throw MobileCoreError.Failed(code: "argument.invalid", domain: "restoration", operation: "scan.failure-recovery.head", detail: "index is invalid")
      }
      let body: [String: Any] = try withScanRecovery(leaseId, operation: "scan.failure-recovery.head") { state in
        guard let owner = state.owners[handleId] else { throw MobileCoreError.Failed(code: "ownership.denied", domain: "restoration", operation: "scan.failure-recovery.head", detail: "owner handle is stale") }
        let key = "\(handleId):\(UInt64(index))"
        guard state.heads.count < Self.maxScanRecoveryAliases || state.headHandles[key] != nil else {
          throw MobileCoreError.Failed(code: "storage.busy", domain: "restoration", operation: "scan.failure-recovery.head", detail: "head alias capacity exceeded")
        }
        guard let head = try owner.head(memberIndex: UInt64(index)) else { return ["head": NSNull()] }
        let info = head.info(); let id = state.headHandles[key] ?? scanRecoveryHandle(); state.headHandles[key] = id; state.heads[id] = head
        return ["head": ["handleId": id, "protocol": info.protocol, "representation": info.representation,
                          "encodedLength": info.encodedLength, "chunkBytes": info.chunkBytes]]
      }
      completion(try scanRecoveryJSON(body), nil)
    } catch { completion(nil, Self.failureJson(error, operation: "scan.failure-recovery.head")) }
  }

  public func scanFailureRecoveryHead(ownerHandle: String, index: Double, completion: @escaping (String?, String?) -> Void) {
    do { try scanFailureRecoveryHead(scanRecoveryLeaseId(ownerHandle: ownerHandle, operation: "scan.failure-recovery.head"), handleId: ownerHandle, index: index, completion: completion) }
    catch { completion(nil, Self.failureJson(error, operation: "scan.failure-recovery.head")) }
  }

  public func scanFailureRecoveryControl(_ leaseId: String, handleId: String, operation: String,
                                         completion: @escaping (String?, String?) -> Void) {
    do {
      let result: [String: Any] = try withScanRecovery(leaseId, operation: "scan.failure-recovery.control") { state in
        guard let head = state.heads[handleId] else { throw MobileCoreError.Failed(code: "ownership.denied", domain: "restoration", operation: "scan.failure-recovery.control", detail: "head handle is stale") }
        switch operation {
        case "acknowledge": try head.acknowledge(); return ["acknowledged": true, "completed": true]
        case "retire":
          try head.retire(); state.heads.removeValue(forKey: handleId); state.headHandles = state.headHandles.filter { $0.value != handleId }; return ["completed": true]
        case "release":
          try head.release(); state.heads.removeValue(forKey: handleId); state.headHandles = state.headHandles.filter { $0.value != handleId }; return ["completed": true]
        case "finalize":
          guard state.cleanup.count < Self.maxScanRecoveryAliases else { throw MobileCoreError.Failed(code: "storage.busy", domain: "restoration", operation: "scan.failure-recovery.control", detail: "cleanup alias capacity exceeded") }
          let receipt = try head.finalize(); let id = scanRecoveryHandle(); state.cleanup[id] = receipt
          state.heads.removeValue(forKey: handleId); state.headHandles = state.headHandles.filter { $0.value != handleId }
          return ["cleanupReceiptId": id]
        default: throw MobileCoreError.Failed(code: "argument.invalid", domain: "restoration", operation: "scan.failure-recovery.control", detail: "unsupported control")
        }
      }
      completion(try scanRecoveryJSON(result), nil)
    } catch { completion(nil, Self.failureJson(error, operation: "scan.failure-recovery.control")) }
  }

  private func scanRecoveryLeaseId(headHandle: String, operation: String) throws -> String {
    scanFailureRecoverySelectionLock.lock()
    let generation = scanFailureRecoverySelectionGeneration
    scanFailureRecoveryHandlesLock.lock(); defer { scanFailureRecoveryHandlesLock.unlock(); scanFailureRecoverySelectionLock.unlock() }
    guard let pair = scanFailureRecoveryHandles.first(where: { $0.value.generation == generation && $0.value.heads[headHandle] != nil }) else {
      throw MobileCoreError.Failed(code: "ownership.denied", domain: "restoration", operation: operation, detail: "head handle is stale")
    }
    return pair.key
  }

  public func scanFailureRecoveryControl(headHandle: String, operation: String, completion: @escaping (String?, String?) -> Void) {
    do { try scanFailureRecoveryControl(scanRecoveryLeaseId(headHandle: headHandle, operation: "scan.failure-recovery.control"), handleId: headHandle, operation: operation, completion: completion) }
    catch { completion(nil, Self.failureJson(error, operation: "scan.failure-recovery.control")) }
  }

  public func scanFailureRecoveryPendingCleanup(_ leaseId: String, completion: @escaping (String?, String?) -> Void) {
    do {
      let ids: [String] = try withScanRecovery(leaseId, operation: "scan.failure-recovery.pending-cleanup") { state in
        if state.cleanupEnumerated { return Array(state.cleanup.keys).sorted() }
        let receipts = try state.lease.pendingCleanupReceipts()
        guard state.cleanup.count + receipts.count <= Self.maxScanRecoveryAliases else { throw MobileCoreError.Failed(code: "storage.busy", domain: "restoration", operation: "scan.failure-recovery.pending-cleanup", detail: "cleanup alias capacity exceeded") }
        for receipt in receipts { state.cleanup[scanRecoveryHandle()] = receipt }
        state.cleanupEnumerated = true
        return Array(state.cleanup.keys).sorted()
      }
      completion(try scanRecoveryJSON(["cleanupReceiptIds": ids]), nil)
    } catch { completion(nil, Self.failureJson(error, operation: "scan.failure-recovery.pending-cleanup")) }
  }

  public func scanFailureRecoveryRetireCleanup(_ leaseId: String, handleId: String, completion: @escaping (String?) -> Void) {
    do {
      try withScanRecovery(leaseId, operation: "scan.failure-recovery.retire-cleanup") { state in
        guard let receipt = state.cleanup[handleId] else { throw MobileCoreError.Failed(code: "ownership.denied", domain: "restoration", operation: "scan.failure-recovery.retire-cleanup", detail: "cleanup handle is stale") }
        try self.testingBeforeScanRecoveryCleanupRetire?()
        try receipt.retire()
        state.cleanup.removeValue(forKey: handleId)
      }
      completion(nil)
    } catch { completion(Self.failureJson(error, operation: "scan.failure-recovery.retire-cleanup")) }
  }

  private func scanRecoveryLeaseId(cleanupHandle: String, operation: String) throws -> String {
    scanFailureRecoverySelectionLock.lock()
    let generation = scanFailureRecoverySelectionGeneration
    scanFailureRecoveryHandlesLock.lock(); defer { scanFailureRecoveryHandlesLock.unlock(); scanFailureRecoverySelectionLock.unlock() }
    guard let pair = scanFailureRecoveryHandles.first(where: { $0.value.generation == generation && $0.value.cleanup[cleanupHandle] != nil }) else {
      throw MobileCoreError.Failed(code: "ownership.denied", domain: "restoration", operation: operation, detail: "cleanup handle is stale")
    }
    return pair.key
  }

  public func scanFailureRecoveryRetireCleanup(handleId: String, completion: @escaping (String?) -> Void) {
    do { try scanFailureRecoveryRetireCleanup(scanRecoveryLeaseId(cleanupHandle: handleId, operation: "scan.failure-recovery.retire-cleanup"), handleId: handleId, completion: completion) }
    catch { completion(Self.failureJson(error, operation: "scan.failure-recovery.retire-cleanup")) }
  }

  /// Used only by the dedicated JSI binary provider. It takes exactly one
  /// native page read while both selection and binding generations are live.
  @objc(readReactNativeScanFailureRecoveryPage:offset:error:)
  public func readReactNativeScanFailureRecoveryPage(_ headHandle: String, offset: String) throws -> ScanFailureRecoveryPage {
    guard offset.range(of: "^(?:0|[1-9][0-9]*)$", options: .regularExpression) != nil else {
      throw MobileCoreError.Failed(code: "argument.invalid", domain: "restoration", operation: "scan.failure-recovery.read", detail: "offset is not canonical")
    }
    let leaseId = try scanRecoveryLeaseId(headHandle: headHandle, operation: "scan.failure-recovery.read")
    return try withScanRecovery(leaseId, operation: "scan.failure-recovery.read") { state in
      guard let head = state.heads[headHandle] else {
        throw MobileCoreError.Failed(code: "ownership.denied", domain: "restoration", operation: "scan.failure-recovery.read", detail: "head handle is stale")
      }
      let page = try head.read(offset: offset)
      guard page.bytes.count <= 64 * 1024 else {
        throw MobileCoreError.Failed(code: "protocol.malformed", domain: "restoration", operation: "scan.failure-recovery.read", detail: "native page exceeds the 64 KiB boundary")
      }
      return ScanFailureRecoveryPage(offset: page.offset, nextOffset: page.nextOffset, done: page.done, bytes: Data(page.bytes))
    }
  }

  private final class RecoveryObserver {
    weak var owner: NSObject?
    let callback: () -> Void
    init(_ owner: NSObject, _ callback: @escaping () -> Void) {
      self.owner = owner
      self.callback = callback
    }
  }
  private var recoveryObservers = [ObjectIdentifier: RecoveryObserver]()

  @objc(addRecoveryObserver:callback:)
  public func addRecoveryObserver(_ owner: NSObject, callback: @escaping () -> Void) {
    lock.lock()
    recoveryObservers[ObjectIdentifier(owner)] = RecoveryObserver(owner, callback)
    lock.unlock()
  }

  @objc(removeRecoveryObserver:)
  public func removeRecoveryObserver(_ owner: NSObject) {
    lock.lock()
    recoveryObservers.removeValue(forKey: ObjectIdentifier(owner))
    lock.unlock()
  }

  private func notifyRecoveryHint() {
    lock.lock()
    recoveryObservers = recoveryObservers.filter { $0.value.owner != nil }
    let callbacks = recoveryObservers.values.map(\.callback)
    lock.unlock()
    for callback in callbacks { callback() }
  }

  private var sessions = [UInt64: Entry]()
  private var ownerStates = [ObjectIdentifier: OwnerState]()
  private var cleanupPending = Set<UInt64>()
  private var disposalFlights = [UInt64: DisposalFlight]()
  private var scheduledRetries = Set<UInt64>()
  private var nextDisposalGeneration: UInt64 = 0
  private var highestIssued: UInt64 = 0
  /// Wakes that raced ahead of their session's registration (Rust admitted
  /// the session, the module has not stored it yet). Bounded by the number
  /// of concurrent opens.
  private var earlyWakes = Set<UInt64>()

  init(
    installer: @escaping Installer,
    openInvoker: @escaping OpenInvoker = { try $0.openSession(owner: $1, expectedWireRevision: $2) },
    disposeInvoker: @escaping DisposeInvoker = { session, completion in
      session.invoke(op: "session.dispose", argsJson: "{}", completion: InvokeCompletion { envelope in
        completion(envelope)
      })
    },
    retryDelay: DispatchTimeInterval = .seconds(2),
    recordingQueue: DispatchQueue = DispatchQueue(label: "com.ubm.continuation.recording")
  ) {
    self.installer = installer
    self.openInvoker = openInvoker
    self.disposeInvoker = disposeInvoker
    self.retryDelay = retryDelay
    self.recordingQueue = recordingQueue
    super.init()
  }

  // MARK: - Host

  /// Installs the process host once for an admitted operation or the configured
  /// native launch bootstrap. Module construction itself does not own a radio.
  @discardableResult
  public func ensureHost() -> String? {
    do {
      _ = try installedHost()
      return nil
    } catch {
      return Self.failureJson(error, operation: "rust-core.host.install")
    }
  }

  private func installedHost() throws -> MobileCoreHost {
    hostLock.lock()
    if let host {
      hostLock.unlock()
      return host
    }
    guard !hostInstalling else {
      hostLock.unlock()
      throw MobileCoreError.Failed(code: "lifecycle.invalid-state", domain: "core",
        operation: "rust-core.host.install", detail: "host installation is already in progress")
    }
    hostInstalling = true
    hostLock.unlock()
    do {
      let installed = try installer(self)
      hostLock.lock()
      host = installed
      hostInstalling = false
      hostLock.unlock()
      return installed
    } catch {
      hostLock.lock()
      hostInstalling = false
      hostLock.unlock()
      throw error
    }
  }

  private func existingHost() -> MobileCoreHost? {
    hostLock.lock()
    defer { hostLock.unlock() }
    return host
  }

  /// Native host bootstrap seam shared by production installation and the
  /// scripted Apple harness. It is Swift-only, so JS cannot register or
  /// replace an adapter/authority.
  func registerProductionRecoveryAdapter(_ adapter: UnifiedBleRustRadioAdapter, for host: MobileCoreHost) {
    hostLock.lock()
    notificationFailureSpoolAdapter = (host, adapter)
    scanFailureRecoveryAdapter = (host, adapter)
    hostLock.unlock()
  }

  private func notificationFailureSpoolAdapter(for host: MobileCoreHost) -> UnifiedBleRustRadioAdapter? {
    hostLock.lock()
    defer { hostLock.unlock() }
    guard let installed = notificationFailureSpoolAdapter, installed.host === host else { return nil }
    return installed.adapter
  }

  private func scanFailureRecoveryAdapter(for host: MobileCoreHost) -> UnifiedBleRustRadioAdapter? {
    hostLock.lock()
    defer { hostLock.unlock() }
    guard let installed = scanFailureRecoveryAdapter, installed.host === host else { return nil }
    return installed.adapter
  }

  private static func isNativeRecordingName(_ value: String) -> Bool {
    !value.isEmpty && value.utf8.count <= 512 && !value.contains("/") &&
      !value.contains("\\") && !value.unicodeScalars.contains(where: { $0.value == 0 })
  }

  /// The process central's configuration, read from the app bundle as the
  /// legacy module read it: the derived restore identifier (restoration
  /// needs it at central creation) and `UnifiedBleProtocolShowPowerAlert`.
  static func productionRadioConfiguration(
    bundle: Bundle
  ) -> (restoreIdentifierKey: String?, showPowerAlert: NSNumber?) {
    (
      UnifiedBleRustRestorationIdentity.configuredRestoreIdentifier(bundle: bundle),
      bundle.object(forInfoDictionaryKey: "UnifiedBleProtocolShowPowerAlert") as? NSNumber
    )
  }

  /// The process radio behind the Expo permission prompt (finding 179): the
  /// same instance the Rust host installs, so the prompt-triggering central
  /// is the one central the process owns.
  @objc(radioForPermissionPrompt)
  public static func radioForPermissionPrompt() -> OwnedCoreBluetoothProtocolRadio {
    let configuration = productionRadioConfiguration(bundle: .main)
    return OwnedCoreBluetoothProtocolRadioOwner.acquire(
      restoreIdentifierKey: configuration.restoreIdentifierKey,
      showPowerAlert: configuration.showPowerAlert
    )
  }

  static func installProductionHost(wake: MobileWakeSink, bundle: Bundle = .main) throws -> MobileCoreHost {
    retryProductionInstallDebt()
    productionDebtLock.lock()
    let available = productionDebt == nil && !productionInstallInProgress
    if available { productionInstallInProgress = true }
    productionDebtLock.unlock()
    guard available else {
      throw MobileCoreError.Failed(code: "lifecycle.invalid-state", domain: "cleanup",
        operation: "rust-core.host.install", detail: "a previous radio binding is installing or awaiting cleanup")
    }
    defer {
      productionDebtLock.lock()
      productionInstallInProgress = false
      productionDebtLock.unlock()
    }
    guard !mobileHostHasPendingCleanup() else {
      throw MobileCoreError.Failed(code: "lifecycle.invalid-state", domain: "cleanup",
        operation: "rust-core.host.install", detail: "an unpublished host is awaiting cleanup")
    }
    let configuration = productionRadioConfiguration(bundle: bundle)
    let radio = OwnedCoreBluetoothProtocolRadioOwner.acquire(
      restoreIdentifierKey: configuration.restoreIdentifierKey,
      showPowerAlert: configuration.showPowerAlert
    )
    // A genuine OS restoration launch must recover its original central.
    // Legacy apps keep eager restoration timing; an ASK app's ordinary launch
    // waits for its actual authorized accessory list before creating a central.
    if OwnedCoreBluetoothProtocolRadioSupport.restorationConfigured(
      restoreIdentifierKey: configuration.restoreIdentifierKey
    ) && OwnedCoreBluetoothProtocolRadioSupport.shouldCreateStartupCentral(
      restorationIdentifier: configuration.restoreIdentifierKey,
      accessorySetup: OwnedCoreBluetoothProtocolRadioSupport.accessorySetupConfigured(info: bundle.infoDictionary ?? [:]),
      restorationLaunchIdentifiers: nativeRestorationLaunchIdentifiers()
    ) {
      // Installation already synchronously attaches the delegate below and
      // must not enter from the radio queue. Allocate on that same queue so
      // permission/snapshot work and initial callbacks cannot race the
      // central's nil/create/assignment sequence.
      dispatchPrecondition(condition: .notOnQueue(radio.queue))
      radio.queue.sync { _ = radio.ensureCentral() }
    }
    let adapter = UnifiedBleRustRadioAdapter(driver: radio, onRestoredPeer: { peer in
      guard let sessions = wake as? UnifiedBleRustCoreSessions else { return }
      sessions.continueRestoredPeer(peer) { _, failure in
        if let failure { NSLog("[UnifiedBleRustCore] native continuation refused: %@", failure) }
      }
    })
    guard radio.attachDelegateIfAvailable(adapter) else {
      throw MobileCoreError.Failed(
        code: "lifecycle.invalid-state",
        domain: "core",
        operation: "rust-core.host.install",
        detail: "another native module already receives the process CoreBluetooth radio's callbacks"
      )
    }
    do {
      let host = try mobileHostInstallBound(
        radio: adapter,
        wake: wake,
        binding: ProductionHostBinding(adapter),
        platform: "apple",
        owner: "unified-ble-manager.react-native.apple",
        adapterLabel: "corebluetooth"
      )
      (wake as? UnifiedBleRustCoreSessions)?.registerProductionRecoveryAdapter(adapter, for: host)
      return host
    } catch {
      if mobileHostHasPendingCleanup() {
        let debt = ProductionInstallDebt(radio: radio, adapter: adapter)
        productionDebtLock.lock()
        productionDebt = debt
        productionDebtLock.unlock()
      } else {
        radio.releaseBorrowerIfCurrent(adapter) { releaseError in
          if let releaseError {
            NSLog("[UnifiedBleRustCoreSessions] releasing the radio after a failed install failed: %@", releaseError)
          }
        }
      }
      throw error
    }
  }

  // MARK: - Sessions

  /// Resolves Rust's admission JSON, or a failure JSON.
  public func openSession(
    _ owner: String,
    expectedWireRevision: String,
    ownerToken: NSObject,
    onWake: @escaping (String) -> Void,
    completion: (String?, String?) -> Void
  ) {
    let ownerKey = ObjectIdentifier(ownerToken)
    lock.lock()
    // The state is retained through a native open, even when module
    // invalidation races the return from Rust admission.
    let ownerState = ownerStates[ownerKey].flatMap { $0.owner === ownerToken ? $0 : nil } ?? OwnerState(ownerToken)
    ownerStates[ownerKey] = ownerState
    if ownerState.invalidated {
      lock.unlock()
      return completion(nil, Self.failureJson(
        code: "lifecycle.destroyed", domain: "lifecycle", operation: "rust-core.open-session",
        detail: "the owning module was invalidated"
      ))
    }
    ownerState.opening += 1
    lock.unlock()
    let session: MobileCoreSession
    do {
      session = try openInvoker(installedHost(), owner, expectedWireRevision)
    } catch {
      lock.lock()
      ownerState.opening -= 1
      pruneOwnerStatesLocked()
      lock.unlock()
      return completion(nil, Self.failureJson(error, operation: "rust-core.open-session"))
    }
    let id = session.sessionId()
    lock.lock()
    ownerState.opening -= 1
    let invalidated = ownerState.invalidated
    sessions[id] = Entry(session: session, owner: ownerToken, ownerState: ownerState, onWake: onWake)
    if invalidated { cleanupPending.insert(id) }
    highestIssued = max(highestIssued, id)
    let wokeEarly = earlyWakes.remove(id) != nil
    lock.unlock()
    if invalidated {
      closeSession(String(id)) { failure in
        if let failure {
          NSLog("[UnifiedBleRustCoreSessions] disposing late session %llu failed: %@", id, failure)
        }
      }
      return completion(nil, Self.failureJson(
        code: "lifecycle.destroyed", domain: "lifecycle", operation: "rust-core.open-session",
        detail: "the owning module was invalidated during admission"
      ))
    }
    if wokeEarly { onWake(String(id)) }
    completion(session.admissionJson(), nil)
  }

  /// Resolves Rust's envelope JSON (a domain failure is data inside it), or
  /// a failure JSON when the session cannot be addressed.
  public func invoke(
    sessionId: String,
    op: String,
    argsJson: String,
    completion: @escaping (String?, String?) -> Void
  ) {
    // Preserve session admission order while recorded storage is configured.
    // Cancellation is allowed ahead so a delayed admission can be cancelled.
    if op == "op.cancel" {
      invokeConfigured(sessionId: sessionId, op: op, argsJson: argsJson, completion: completion)
      return
    }
    invokeQueue.async {
      do {
        if op == "gatt.subscribe-recorded" { try self.configureRecordingStorage() }
        self.invokeConfigured(sessionId: sessionId, op: op, argsJson: argsJson, completion: completion)
      } catch { completion(nil, Self.recordingFailureJson(error)) }
    }
  }

  private func invokeConfigured(
    sessionId: String, op: String, argsJson: String,
    completion: @escaping (String?, String?) -> Void
  ) {
    switch entry(sessionId, operation: "rust-core.invoke") {
    case let .failure(failure):
      completion(nil, failure)
    case let .success(entry):
      entry.session.invoke(op: op, argsJson: argsJson, completion: InvokeCompletion { envelope in
        if (op == "session.dispose" || op == "gatt.unsubscribe") && Self.disposeFailure(envelope) == nil {
          self.notifyRecoveryHint()
        }
        completion(envelope, nil)
      })
    }
  }

  public func drain(
    sessionId: String,
    maxItems: Double,
    maxBytes: Double,
    completion: (String?, String?) -> Void
  ) {
    guard let items = Self.positiveUInt32(maxItems), let bytes = Self.positiveUInt32(maxBytes) else {
      return completion(nil, Self.failureJson(
        code: "argument.invalid", domain: "core", operation: "rust-core.drain",
        detail: "maxItems and maxBytes must be integers in 1...4294967295"
      ))
    }
    switch entry(sessionId, operation: "rust-core.drain") {
    case let .failure(failure):
      completion(nil, failure)
    case let .success(entry):
      completion(entry.session.drain(maxItems: items, maxBytes: bytes), nil)
    }
  }

  /// Disposes the session through Rust (idempotent there: a second dispose
  /// of a released session releases nothing), then forgets the lease. A
  /// dispose that did not release keeps the lease so a retry can finish it.
  public func closeSession(_ sessionId: String, completion: @escaping (String?) -> Void) {
    guard let id = Self.sessionNumber(sessionId) else {
      return completion(Self.failureJson(
        code: "argument.invalid", domain: "core", operation: "rust-core.close-session",
        detail: "sessionId is not a decimal session id"
      ))
    }
    lock.lock()
    let held = sessions[id]
    if held != nil, var flight = disposalFlights[id] {
      flight.completions.append(completion)
      disposalFlights[id] = flight
      lock.unlock()
      return
    }
    if held != nil {
      nextDisposalGeneration &+= 1
      disposalFlights[id] = DisposalFlight(generation: nextDisposalGeneration, completions: [completion])
    }
    let generation = nextDisposalGeneration
    lock.unlock()
    guard let held else { return completion(nil) }
    disposeInvoker(held.session) { envelope in
      self.finishDisposal(id: id, generation: generation, envelope: envelope)
    }
  }

  /// Module teardown (React reload): the JS owner of these sessions is gone.
  @objc(closeSessionsOwnedBy:)
  public func closeSessions(ownedBy ownerToken: NSObject) {
    lock.lock()
    let ownerKey = ObjectIdentifier(ownerToken)
    let ownerState = ownerStates[ownerKey].flatMap { $0.owner === ownerToken ? $0 : nil } ?? OwnerState(ownerToken)
    ownerStates[ownerKey] = ownerState
    ownerState.invalidated = true
    let owned = sessions.filter { $0.value.ownerState === ownerState || $0.value.owner == nil }.map(\.key)
    cleanupPending.formUnion(owned)
    pruneOwnerStatesLocked()
    lock.unlock()
    for id in owned {
      closeSession(String(id)) { failure in
        if let failure {
          NSLog("[UnifiedBleRustCoreSessions] disposing session %llu at module teardown failed: %@", id, failure)
        }
      }
    }
  }

  private func finishDisposal(id: UInt64, generation: UInt64, envelope: String) {
    let failure = Self.disposeFailure(envelope)
    lock.lock()
    guard let flight = disposalFlights[id], flight.generation == generation else {
      lock.unlock()
      return
    }
    disposalFlights.removeValue(forKey: id)
    if failure == nil {
      sessions.removeValue(forKey: id)
      cleanupPending.remove(id)
      scheduledRetries.remove(id)
      pruneOwnerStatesLocked()
    }
    let shouldRetry = failure != nil && cleanupPending.contains(id)
    lock.unlock()
    if failure == nil { notifyRecoveryHint() }
    for callback in flight.completions { callback(failure) }
    if shouldRetry { scheduleRetry(id) }
  }

  private func scheduleRetry(_ id: UInt64) {
    lock.lock()
    guard cleanupPending.contains(id), sessions[id] != nil, scheduledRetries.insert(id).inserted else {
      lock.unlock()
      return
    }
    lock.unlock()
    DispatchQueue.global(qos: .utility).asyncAfter(deadline: .now() + retryDelay) { [self] in
      lock.lock()
      scheduledRetries.remove(id)
      let shouldRetry = cleanupPending.contains(id) && sessions[id] != nil
      lock.unlock()
      if shouldRetry {
        closeSession(String(id)) { failure in
          if let failure {
            NSLog("[UnifiedBleRustCoreSessions] retrying session %llu disposal failed: %@", id, failure)
          }
        }
      }
    }
  }

  /// Call with lock held. Only module states with neither accepted work nor
  /// retained sessions can be removed; an in-flight open preserves its state.
  private func pruneOwnerStatesLocked() {
    ownerStates = ownerStates.filter { _, state in
      state.opening > 0 || sessions.values.contains { $0.ownerState === state } || state.owner != nil
    }
  }

  // MARK: - MobileWakeSink

  public func wake(sessionId: UInt64) {
    lock.lock()
    let entry = sessions[sessionId]
    if entry == nil, sessionId > highestIssued {
      earlyWakes.insert(sessionId)
    }
    lock.unlock()
    guard let entry else {
      if sessionId <= highestIssued {
        NSLog("[UnifiedBleRustCoreSessions] wake for closed session %llu", sessionId)
      }
      return
    }
    entry.onWake(String(sessionId))
  }

  // MARK: - Rust-answered identities

  public func nativeBuildIdentity() -> String { mobileBuildIdentityJson() }

  /// Reads the OS-saved ASK list without installing a BLE host, radio or
  /// picker. The process-owned AS session survives a JS module reload.
  public func authorizedAccessories(_ completion: @escaping (String?, String?) -> Void) {
    OwnedCoreBluetoothProtocolRadioSupport.queryAuthorizedAccessoryList(completion: { result in
      switch result {
      case .success(let text): completion(text, nil)
      case .failure(let error):
        completion(nil, error.domain == "UnifiedBleAccessoryStartup" && error.code == 3
          ? Self.failureJson(code: "capability.unsupported", domain: "capability",
            operation: "accessory.authorized", detail: error.localizedDescription)
          : Self.platformFailureJson(error,
            operation: "accessory.authorized", detail: error.localizedDescription))
      }
    }, sessionFailure: { error in
      self.recordAccessoryStartupFailure(error)
    })
  }
  public func contractRevision() -> String { mobileContractRevision() }
  public func wireRevision() -> String { mobileWireRevision() }

  // MARK: - Host facilities

  /// `length` cryptographically secure bytes as strict padded base64.
  public func randomBytes(_ length: Double, completion: (String?, String?) -> Void) {
    guard let count = Int(exactly: length), (1...1024).contains(count) else {
      return completion(nil, Self.failureJson(
        code: "argument.invalid", domain: "core", operation: "rust-core.random-bytes",
        detail: "length must be an integer in 1...1024"
      ))
    }
    var bytes = [UInt8](repeating: 0, count: count)
    guard SecRandomCopyBytes(kSecRandomDefault, count, &bytes) == errSecSuccess else {
      return completion(nil, Self.failureJson(
        code: "capability.unsupported", domain: "capability", operation: "rust-core.random-bytes",
        detail: "SecRandomCopyBytes failed"
      ))
    }
    completion(Data(bytes).base64EncodedString(), nil)
  }

  public func restorationIdentity(_ requestJson: String, completion: (String?, String?) -> Void) {
    switch UnifiedBleRustRestorationIdentity.bootstrap(requestJson: requestJson, bundle: .main) {
    case let .success(identity): completion(identity, nil)
    case let .failure(failure): completion(nil, failure.json)
    }
  }

  // MARK: - Process-owned background continuation

  /// Called by the native launch observer independently of TurboModule/JS
  /// creation. A configured restoration identity opts into record-only startup;
  /// a native standing order additionally opts into autonomous recovery.
  /// Merely installing UBM without either configuration remains inert.
  public func bootstrapNativeContinuation() -> String? {
    bootstrapNativeContinuation(bundle: .main)
  }

  @nonobjc
  func bootstrapNativeContinuation(bundle: Bundle) -> String? {
    var strategy = "record-only"
    if let declaration = Self.continuationDeclaration(bundle: bundle) {
      switch Self.validatedContinuation(declaration) {
      case .failure(let error):
        Self.countMalformedDeclaration(declaration)
        return Self.failureJson(code: "argument.invalid", domain: "restoration",
                                operation: "continuation.bootstrap", detail: error.detail)
      case .success(let valid): strategy = valid.strategy
      }
    }
    let configuration = Self.productionRadioConfiguration(bundle: bundle)
    guard configuration.restoreIdentifierKey != nil else {
      if strategy != "native" { return nil }
      return Self.failureJson(code: "capability.unsupported", domain: "restoration",
                              operation: "continuation.bootstrap", detail: "native continuation requires a configured restoration identifier")
    }
    #if os(tvOS)
    return Self.failureJson(code: "capability.unsupported", domain: "restoration",
                            operation: "continuation.bootstrap", detail: "tvOS does not provide Bluetooth state restoration")
    #else
    if let failure = ensureHost() { return failure }
    let accessorySetup = OwnedCoreBluetoothProtocolRadioSupport.accessorySetupConfigured(info: bundle.infoDictionary ?? [:])
    if OwnedCoreBluetoothProtocolRadioSupport.shouldCreateStartupCentral(
      restorationIdentifier: configuration.restoreIdentifierKey, accessorySetup: accessorySetup,
      restorationLaunchIdentifiers: Self.nativeRestorationLaunchIdentifiers()
    ) {
      // A module can install/bind the host before the launch notification
      // arrives. Recheck the now-known OS launch identity even for that host.
      let radio = OwnedCoreBluetoothProtocolRadioOwner.acquire(
        restoreIdentifierKey: configuration.restoreIdentifierKey,
        showPowerAlert: configuration.showPowerAlert
      )
      radio.queue.async { _ = radio.ensureCentral() }
    } else if accessorySetup {
      resumeAuthorizedAccessoryStartup(configuration)
    }
    return nil
    #endif
  }

  private func resumeAuthorizedAccessoryStartup(
    _ configuration: (restoreIdentifierKey: String?, showPowerAlert: NSNumber?)
  ) {
    lock.lock()
    guard !accessoryStartupQueryInFlight else { lock.unlock(); return }
    accessoryStartupQueryInFlight = true
    lock.unlock()
    // ensureHost above has already attached and bound the event sink. An
    // immediate authorization callback cannot precede that ownership boundary.
    OwnedCoreBluetoothProtocolRadioSupport.resumeAuthorizedAccessoryStartup(
      query: { completion in
        OwnedCoreBluetoothProtocolRadioSupport.queryAuthorizedAccessories(completion: { result in
          self.lock.lock()
          switch result {
          case .success: self.accessoryStartupFailure = nil
          case .failure(let error):
            self.accessoryStartupFailure = Self.platformFailureJson(error,
              operation: "continuation.bootstrap.accessory-authorization", detail: error.localizedDescription)
          }
          self.accessoryStartupQueryInFlight = false
          self.lock.unlock()
          completion(result)
        }, sessionFailure: { error in
          self.recordAccessoryStartupFailure(error)
        })
      },
      createCentral: {
        let radio = OwnedCoreBluetoothProtocolRadioOwner.acquire(
          restoreIdentifierKey: configuration.restoreIdentifierKey,
          showPowerAlert: configuration.showPowerAlert
        )
        radio.queue.async { _ = radio.ensureCentral() }
      },
      failure: { error in
        self.recordAccessoryStartupFailure(error)
      }
    )
  }

  private func recordAccessoryStartupFailure(_ error: NSError) {
    let failure = Self.platformFailureJson(error, operation: "continuation.bootstrap.accessory-authorization",
      detail: error.localizedDescription)
    lock.lock()
    accessoryStartupFailure = failure
    lock.unlock()
    NSLog("[UnifiedBleRustCore] native accessory authorization bootstrap failed: %@", failure)
  }

  /// The restored peer is supplied by the process radio, never a JS callback.
  /// Execution, subscription ownership, bounded buffering and handoff all
  /// remain in the shared Rust executor used by both native platforms.
  public func continueRestoredPeer(_ peer: String, completion: @escaping (String?, String?) -> Void) {
    continuationDeclarationGate.lock()
    defer { continuationDeclarationGate.unlock() }
    guard let declaration = Self.continuationDeclaration() else {
      return completion("{\"state\":\"record-only\"}", nil)
    }
    switch Self.validatedContinuation(declaration) {
    case .failure(let error):
      Self.countMalformedDeclaration(declaration)
      return completion(nil, Self.failureJson(code: "argument.invalid", domain: "restoration",
                                               operation: "continuation.execute", detail: error.detail))
    case .success(let valid):
      guard valid.strategy != "record-only" else { return completion("{\"state\":\"record-only\"}", nil) }
      guard valid.peerId == nil || valid.peerId == peer.uppercased() else {
        let failure = Self.failureJson(code: "operation.aborted", domain: "restoration",
          operation: "continuation.execute", detail: "native continuation skips \(peer): standing order scopes to \(valid.peerId ?? "")")
        return Self.finishContinuation(strategy: valid.strategy, peer: peer, value: nil, failure: failure, completion: completion)
      }
      guard valid.strategy == "native" else {
        let failure = Self.failureJson(code: "capability.unsupported", domain: "restoration",
          operation: "continuation.execute", detail: "\(valid.strategy) continuation is unavailable on Apple hosts")
        Self.finishContinuation(strategy: valid.strategy, peer: peer, value: nil, failure: failure, completion: completion)
        return
      }
      executeNativeContinuation(peer, declarationJson: declaration) { envelope in
        Self.unwrapContinuation(envelope, operation: "continuation.execute") { value, failure in
          Self.finishContinuation(strategy: valid.strategy, peer: peer, value: value, failure: failure, completion: completion)
        }
      }
    }
  }

  /// Trusted native warm execution on the existing process owner, not an OS
  /// restoration callback. The caller must explicitly declare matching authority
  /// first; this does not persist a standing order or manufacture a wake record.
  /// Native results (including seed refusals) retain their canonical envelope.
  public func executeNativeContinuation(_ peerId: String, declarationJson: String,
                                        completion: @escaping (String) -> Void) {
    guard !declarationJson.isEmpty, declarationJson.utf8.count <= Self.maxContinuationJson else {
      return completion(Self.continuationFailureEnvelope(Self.failureJson(code: "argument.invalid", domain: "restoration",
        operation: "continuation.execute", detail: "declaration must be 1..\(Self.maxContinuationJson) bytes")))
    }
    switch Self.validatedContinuation(declarationJson) {
    case .failure(let error):
      return completion(Self.continuationFailureEnvelope(Self.failureJson(code: "argument.invalid", domain: "restoration",
        operation: "continuation.execute", detail: error.detail)))
    case .success(let valid):
      guard valid.strategy == "native" else {
        return completion(Self.continuationFailureEnvelope(Self.failureJson(code: "capability.unsupported", domain: "restoration",
          operation: "continuation.execute", detail: "warm execution requires a native continuation declaration")))
      }
      if valid.recording && Thread.isMainThread {
        recordingQueue.async { self.executeNativeContinuation(peerId, declarationJson: declarationJson, completion: completion) }
        return
      }
      continuationDeclarationGate.lock()
      defer { continuationDeclarationGate.unlock() }
      // Persisted app/bundle policy is the authority, even before Rust has
      // committed its first order. A warm caller may not seed its own policy.
      guard let standingOrder = Self.continuationDeclaration(),
            standingOrder.utf8.count <= Self.maxContinuationJson,
            let persisted = try? JSONSerialization.jsonObject(with: Data(standingOrder.utf8)) as? NSDictionary,
            let requested = try? JSONSerialization.jsonObject(with: Data(declarationJson.utf8)) as? NSDictionary,
            persisted == requested else {
        return completion(Self.continuationFailureEnvelope(Self.failureJson(code: "lifecycle.invalid-state", domain: "restoration",
          operation: "continuation.execute", detail: "warm execution requires a matching persisted or bundle standing order")))
      }
      do {
        if valid.recording { try configureRecordingStorage() }
        let owner = try installedHost()
        let seed = owner.continuationSeedDeclaration(declarationJson: declarationJson)
        var seedFailure: String?
        Self.unwrapContinuation(seed, operation: "continuation.execute") { _, failure in seedFailure = failure }
        guard seedFailure == nil else { return completion(seed) }
        owner.continuationExecute(peerId: peerId, declarationJson: declarationJson, completion: InvokeCompletion(completion))
      } catch {
        if let failure = error as? RecordingFailure {
          return completion(failure.envelope ?? Self.continuationFailureEnvelope(failure.json))
        }
        completion(Self.continuationFailureEnvelope(Self.failureJson(error, operation: "continuation.execute")))
      }
    }
  }

  /// Canonical process backlog, not the OS restoration/posture summary.
  public func describeNativeContinuation(completion: @escaping (String) -> Void) {
    let installed = existingHost()
    guard let installed else { return completion("{\"ok\":true,\"value\":null}") }
    installed.continuationDescribeBacklog(completion: InvokeCompletion(completion))
  }

  /// Preparing freezes a replayable handoff; ownership remains native until ACK.
  public func prepareNativeContinuationClaim(maxItems: Double, maxBytes: Double, completion: @escaping (String) -> Void) {
    guard let items = Self.positiveUInt32(maxItems), let bytes = Self.positiveUInt32(maxBytes) else {
      return completion(Self.continuationFailureEnvelope(Self.failureJson(code: "argument.invalid", domain: "restoration",
        operation: "continuation.claim", detail: "claim bounds must be positive UInt32 integers")))
    }
    guard let installed = existingHost() else {
      return completion(Self.continuationFailureEnvelope(Self.failureJson(code: "lifecycle.invalid-state", domain: "restoration",
        operation: "continuation.claim", detail: "no native continuation process host is installed")))
    }
    installed.continuationPrepareClaim(maxItems: items, maxBytes: bytes, completion: InvokeCompletion(completion))
  }

  public func acknowledgeNativeContinuationClaim(_ claimToken: String, completion: @escaping (String) -> Void) {
    guard let installed = existingHost() else {
      return completion(Self.continuationFailureEnvelope(Self.failureJson(code: "lifecycle.invalid-state", domain: "restoration",
        operation: "continuation.claim", detail: "no native continuation process host is installed")))
    }
    installed.continuationAcknowledgeClaim(claimToken: claimToken, completion: InvokeCompletion(completion))
  }

  // Only wrapper-owned failures need encoding. Native envelopes pass through.
  private static func continuationFailureEnvelope(_ failure: String) -> String {
    "{\"ok\":false,\"error\":\(failure),\"commit\":null,\"retryability\":\"never\"}"
  }

  private static func finishContinuation(strategy: String, peer: String, value: String?, failure: String?,
                                         completion: (String?, String?) -> Void) {
    do {
      let raw = failure ?? value ?? "{}"
      guard let record = try JSONSerialization.jsonObject(with: Data(raw.utf8)) as? [String: Any] else {
        return completion(nil, failureJson(code: "protocol.malformed", domain: "restoration",
          operation: "continuation.execute", detail: "unreadable native outcome"))
      }
      let outcome: [String: Any] = [
        "observedAtMs": Int64(Date().timeIntervalSince1970 * 1000),
        "event": failure == nil ? "continuation.completed" : "continuation.failed",
        "strategy": strategy, "peerAddress": peer,
        "code": failure == nil ? NSNull() : record["code"] ?? NSNull(),
        "reason": failure == nil ? NSNull() : record["detail"] ?? raw
      ]
      let encoded = try JSONSerialization.data(withJSONObject: outcome, options: [.sortedKeys])
      UserDefaults.standard.set(String(decoding: encoded, as: UTF8.self), forKey: continuationLastWakeKey)
      completion(value, failure)
    } catch { completion(nil, failureJson(error, operation: "continuation.outcome.persist")) }
  }

  private static func continuationDeclaration(bundle: Bundle = .main) -> String? {
    UserDefaults.standard.string(forKey: continuationDefaultsKey)
      ?? bundle.object(forInfoDictionaryKey: "UnifiedBleBackgroundContinuation") as? String
  }

  private static func unwrapContinuation(
    _ envelope: String, operation: String, completion: (String?, String?) -> Void
  ) {
    do {
      guard let data = envelope.data(using: .utf8),
            let root = try JSONSerialization.jsonObject(with: data) as? [String: Any],
            let ok = root["ok"] as? Bool,
            let payload = root[ok ? "value" : "error"] as? [String: Any] else {
        return completion(nil, failureJson(code: "protocol.malformed", domain: "core",
                                           operation: operation, detail: "unreadable continuation envelope"))
      }
      let encoded = try JSONSerialization.data(withJSONObject: payload, options: [.sortedKeys])
      let text = String(decoding: encoded, as: UTF8.self)
      completion(ok ? text : nil, ok ? nil : text)
    } catch { completion(nil, failureJson(error, operation: operation)) }
  }

  /// Persists the validated standing order for the native launch path.
  public func declareBackgroundContinuation(_ declarationJson: String, completion: @escaping (String?, String?) -> Void) {
    guard !declarationJson.isEmpty, declarationJson.utf8.count <= Self.maxContinuationJson else {
      completion(nil, Self.failureJson(
        code: "argument.invalid", domain: "restoration", operation: "continuation.declare",
        detail: "declaration must be 1..\(Self.maxContinuationJson) bytes"
      ))
      return
    }
    switch Self.validatedContinuation(declarationJson) {
    case .success(let valid):
      if valid.recording && Thread.isMainThread {
        recordingQueue.async { self.declareBackgroundContinuation(declarationJson, completion: completion) }
        return
      }
      if valid.recording {
        do { try configureRecordingStorage() }
        catch { return completion(nil, Self.failureJson(error, operation: "continuation.recording.configure")) }
      }
      continuationDeclarationGate.lock()
      defer { continuationDeclarationGate.unlock() }
      let installed = existingHost()
      var token: String?
      if let installed {
        var reserveFailure: String?
        Self.unwrapContinuation(installed.continuationReserveDeclaration(declarationJson: declarationJson), operation: "continuation.declare") { value, failure in
          reserveFailure = failure
          if let value, let record = try? JSONSerialization.jsonObject(with: Data(value.utf8)) as? [String: Any] {
            token = record["reservationToken"] as? String
          }
        }
        if let reserveFailure { return completion(nil, reserveFailure) }
        guard token != nil else {
          return completion(nil, Self.failureJson(code: "protocol.malformed", domain: "core",
            operation: "continuation.declare", detail: "missing declaration reservation"))
        }
      }
      // UserDefaults.set has no failure return. The owner reservation blocks
      // stale execution until this process-visible persisted order is committed.
      UserDefaults.standard.set(declarationJson, forKey: Self.continuationDefaultsKey)
      if let installed, let token {
        var commitFailure: String?
        Self.unwrapContinuation(installed.continuationCommitDeclaration(reservationToken: token), operation: "continuation.declare") {
          _, failure in commitFailure = failure
        }
        if let commitFailure { return completion(nil, commitFailure) }
      }
      completion("{\"state\":\"declared\"}", nil)
    case .failure(let error):
      completion(nil, Self.failureJson(
        code: "argument.invalid", domain: "restoration", operation: "continuation.declare",
        detail: error.detail
      ))
    }
  }

  /// Reports the continuation posture for Diagnostics: the validated
  /// declared strategy and peer, how many persisted declarations could not
  /// be parsed, and the last wake outcome. A malformed persisted record
  /// (written before validation existed) falls back to `record-only` and is
  /// counted once per distinct payload — reported, never silently kept. A
  /// unimplemented strategy carries an explicit limitation in `detail`;
  /// `lastWake` is null
  /// until a wake executes one, never invented.
  public func continuationStatus(_ completion: @escaping (String?, String?) -> Void) {
    lock.lock()
    let startupFailure = accessoryStartupFailure
    lock.unlock()
    var strategy = "record-only"
    var peerId: String?
    var resubscribe = 0
    if let stored = Self.continuationDeclaration() {
      switch Self.validatedContinuation(stored) {
      case let .success(valid):
        strategy = valid.strategy
        peerId = valid.peerId
        resubscribe = valid.resubscribe
      case .failure:
        Self.countMalformedDeclaration(stored)
      }
    }
    var status: [String: Any] = [
      "strategy": strategy,
      "peerId": peerId as Any? ?? NSNull(),
      "resubscribe": resubscribe,
      "malformedDeclarations": UserDefaults.standard.integer(forKey: Self.continuationMalformedCountKey),
      "lastWake": Self.readLastWake() as Any? ?? NSNull(),
      "lastRecovery": NSNull()
    ]
    if strategy != "record-only" && strategy != "native" {
      status["detail"] = "\(strategy) is Android-specific; Apple does not provide that task/service mechanism"
    }
    let finish: ([String: Any]) -> Void = { value in
      do {
        let reported = try Self.continuationStatusWithStartupFailure(value, failure: startupFailure)
        let data = try JSONSerialization.data(withJSONObject: reported, options: [.sortedKeys])
        completion(String(decoding: data, as: UTF8.self), nil)
      } catch { completion(nil, Self.failureJson(error, operation: "continuation.status")) }
    }
    let installed = existingHost()
    guard let installed else { return finish(status) }
    let snapshot = status
    installed.continuationDescribeBacklog(completion: InvokeCompletion { envelope in
      do {
        guard let root = try JSONSerialization.jsonObject(with: Data(envelope.utf8)) as? [String: Any],
              let ok = root["ok"] as? Bool else {
          return completion(nil, Self.failureJson(code: "protocol.malformed", domain: "core",
            operation: "continuation.status", detail: "unreadable continuation status"))
        }
        guard ok else { return Self.unwrapContinuation(envelope, operation: "continuation.status", completion: completion) }
        var updated = snapshot
        if let value = root["value"] as? [String: Any] {
          updated["lastRecovery"] = value["continuationOutcome"] ?? NSNull()
        } else if !(root["value"] is NSNull) {
          return completion(nil, Self.failureJson(code: "protocol.malformed", domain: "core",
            operation: "continuation.status", detail: "unreadable continuation counters"))
        }
        finish(updated)
      } catch { completion(nil, Self.failureJson(error, operation: "continuation.status")) }
    })
  }

  static func continuationStatusWithStartupFailure(_ value: [String: Any], failure: String?) throws -> [String: Any] {
    var status = value
    if let failure {
      status["startupFailure"] = try JSONSerialization.jsonObject(with: Data(failure.utf8))
    } else {
      status["startupFailure"] = NSNull()
    }
    return status
  }

  public func prepareContinuationClaim(maxItems: Double, maxBytes: Double, completion: @escaping (String?, String?) -> Void) {
    prepareNativeContinuationClaim(maxItems: maxItems, maxBytes: maxBytes) { envelope in
      Self.unwrapContinuation(envelope, operation: "continuation.claim", completion: completion)
    }
  }

  public func acknowledgeContinuationClaim(_ claimToken: String, completion: @escaping (String?, String?) -> Void) {
    acknowledgeNativeContinuationClaim(claimToken) { envelope in
      Self.unwrapContinuation(envelope, operation: "continuation.claim", completion: completion)
    }
  }

  public func prepareRetainedContinuationClaimV1(maxItems: Double, maxBytes: Double, completion: @escaping (String?, String?) -> Void) {
    let operation = "continuation.claim.v1"
    guard let items = Self.positiveUInt32(maxItems), let bytes = Self.positiveUInt32(maxBytes) else {
      return completion(nil, Self.failureJson(code: "argument.invalid", domain: "restoration", operation: operation,
        detail: "claim bounds must be positive UInt32 integers"))
    }
    guard let installed = existingHost() else {
      return completion(nil, Self.failureJson(code: "lifecycle.invalid-state", domain: "restoration", operation: operation,
        detail: "no native continuation process host is installed"))
    }
    installed.continuationPrepareRetainedClaimV1(maxItems: items, maxBytes: bytes, completion: InvokeCompletion { envelope in
      Self.unwrapContinuation(envelope, operation: operation, completion: completion)
    })
  }

  public func acknowledgeRetainedContinuationClaimV1(_ claimToken: String, completion: @escaping (String?, String?) -> Void) {
    retainedContinuationClaimControlV1(claimToken, retire: false, completion: completion)
  }

  /// Explicit V2 retained-prefix operation. It is intentionally separate from
  /// V1 so an older JavaScript caller cannot observe or acknowledge a V2
  /// descriptor by accident.
  public func prepareRetainedContinuationClaimV2(maxItems: Double, maxBytes: Double, completion: @escaping (String?, String?) -> Void) {
    let operation = "continuation.claim.v2"
    guard let items = Self.positiveUInt32(maxItems), let bytes = Self.positiveUInt32(maxBytes) else {
      return completion(nil, Self.failureJson(code: "argument.invalid", domain: "restoration", operation: operation,
        detail: "claim bounds must be positive UInt32 integers"))
    }
    guard let installed = existingHost() else {
      return completion(nil, Self.failureJson(code: "lifecycle.invalid-state", domain: "restoration", operation: operation,
        detail: "no native continuation process host is installed"))
    }
    installed.continuationPrepareClaimVersioned(version: 2, maxItems: items, maxBytes: bytes, completion: InvokeCompletion { envelope in
      Self.unwrapContinuation(envelope, operation: operation, completion: completion)
    })
  }

  public func acknowledgeRetainedContinuationClaimV2(_ claimToken: String, completion: @escaping (String?, String?) -> Void) {
    let operation = "continuation.claim.v2"
    guard !claimToken.isEmpty, claimToken.utf8.count <= 256 else {
      return completion(nil, Self.failureJson(code: "argument.invalid", domain: "restoration", operation: operation,
        detail: "claimToken must be 1..256 bytes"))
    }
    guard let installed = existingHost() else {
      return completion(nil, Self.failureJson(code: "lifecycle.invalid-state", domain: "restoration", operation: operation,
        detail: "no native continuation process host is installed"))
    }
    installed.continuationAcknowledgeRetainedClaimV2(claimToken: claimToken, completion: InvokeCompletion { envelope in
      Self.unwrapContinuation(envelope, operation: operation, completion: completion)
    })
  }

  public func readPeerFailureRecovery(_ recoveryRecord: String, offset: String, completion: @escaping (String?, String?) -> Void) {
    let operation = "continuation.peer-source-failure.read"
    guard !recoveryRecord.isEmpty, recoveryRecord.utf8.count <= 1_048_576,
          UInt64(offset).map({ String($0) == offset }) == true else {
      return completion(nil, Self.failureJson(code: "argument.invalid", domain: "gatt", operation: operation,
        detail: "recovery record or offset is invalid"))
    }
    guard let installed = existingHost() else {
      return completion(nil, Self.failureJson(code: "lifecycle.invalid-state", domain: "restoration", operation: operation,
        detail: "no native continuation process host is installed"))
    }
    installed.continuationReadPeerFailureRecovery(recoveryRecord: recoveryRecord, offset: offset, completion: InvokeCompletion { envelope in
      Self.unwrapContinuation(envelope, operation: operation, completion: completion)
    })
  }

  public func acknowledgePeerFailureRecovery(_ recoveryRecord: String, completion: @escaping (String?, String?) -> Void) {
    let operation = "continuation.peer-source-failure.acknowledge"
    guard !recoveryRecord.isEmpty, recoveryRecord.utf8.count <= 1_048_576 else {
      return completion(nil, Self.failureJson(code: "argument.invalid", domain: "gatt", operation: operation,
        detail: "recovery record is invalid"))
    }
    guard let installed = existingHost() else {
      return completion(nil, Self.failureJson(code: "lifecycle.invalid-state", domain: "restoration", operation: operation,
        detail: "no native continuation process host is installed"))
    }
    installed.continuationAcknowledgePeerFailureRecovery(recoveryRecord: recoveryRecord, completion: InvokeCompletion { envelope in
      Self.unwrapContinuation(envelope, operation: operation, completion: completion)
    })
  }

  public func retireRetainedContinuationClaimReceiptV1(_ claimToken: String, completion: @escaping (String?, String?) -> Void) {
    retainedContinuationClaimControlV1(claimToken, retire: true, completion: completion)
  }

  private func retainedContinuationClaimControlV1(_ token: String, retire: Bool, completion: @escaping (String?, String?) -> Void) {
    let operation = "continuation.claim.v1"
    guard !token.isEmpty, token.utf8.count <= 256 else {
      return completion(nil, Self.failureJson(code: "argument.invalid", domain: "restoration", operation: operation,
        detail: "claimToken must be 1..256 bytes"))
    }
    guard let installed = existingHost() else {
      return completion(nil, Self.failureJson(code: "lifecycle.invalid-state", domain: "restoration", operation: operation,
        detail: "no native continuation process host is installed"))
    }
    let callback = InvokeCompletion { envelope in Self.unwrapContinuation(envelope, operation: operation, completion: completion) }
    if retire { installed.continuationRetireClaimReceipt(claimToken: token, completion: callback) }
    else { installed.continuationAcknowledgeRetainedClaimV1(claimToken: token, completion: callback) }
  }

  private static let continuationDefaultsKey = "com.sfourdrinier.unifiedblemanager.background-continuation"
  private static let continuationMalformedCountKey = "com.sfourdrinier.unifiedblemanager.background-continuation.malformed-count"
  private static let continuationMalformedPayloadKey = "com.sfourdrinier.unifiedblemanager.background-continuation.malformed-payload"
  private static let continuationLastWakeKey = "com.sfourdrinier.unifiedblemanager.background-continuation.last-wake"
  /// The canonical declaration JSON is small; anything larger is not ours
  /// (the Android declare refuses the same bound).
  private static let maxContinuationJson = 65536

  /// What was actually declared and validated: the strategy, the scoped
  /// peer, and how many resubscriptions the wake would run.
  struct ValidatedContinuation {
    let strategy: String
    let peerId: String?
    let resubscribe: Int
    let recording: Bool
  }

  struct ContinuationValidationError: Error {
    let detail: String
  }

  /// Validates a declaration with the same rules the Android wake enforces:
  /// the exact key set the binding persists, a known strategy, a native peer,
  /// at most 64 well-formed selectors, and the headless-task /
  /// foreground-service payloads only on their own strategies.
  static func validatedContinuation(_ json: String) -> Result<ValidatedContinuation, ContinuationValidationError> {
    func fail(_ detail: String) -> Result<ValidatedContinuation, ContinuationValidationError> {
      .failure(ContinuationValidationError(detail: detail))
    }
    guard let data = json.data(using: .utf8),
          let root = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else {
      return fail("background.continuation: not an object")
    }
    let topKeys: Set<String> = ["onAppearance", "peerId", "resubscribe", "setup", "link", "recording", "headlessTaskName", "foregroundService"]
    let unknown = Set(root.keys).subtracting(topKeys).sorted()
    if !unknown.isEmpty { return fail("background.continuation unknown keys: \(unknown.joined(separator: ","))") }
    let strategies = ["record-only", "native", "headless-task", "foreground-service"]
    let strategy: String
    if root["onAppearance"] == nil {
      strategy = "record-only"
    } else if let wire = root["onAppearance"] as? String, strategies.contains(wire) {
      strategy = wire
    } else {
      return fail("background.continuation: unknown onAppearance \(String(describing: root["onAppearance"]))")
    }
    let peerId: String?
    if root["peerId"] == nil {
      peerId = nil
    } else if let text = root["peerId"] as? String, isMacAddress(text) || isUuid(text) {
      peerId = text.uppercased()
    } else {
      return fail("background.continuation: peerId must be a MAC address or canonical UUID")
    }
    let resubscribe: Int
    if root["resubscribe"] == nil {
      resubscribe = 0
    } else if let entries = root["resubscribe"] as? [Any] {
      if entries.count > 64 { return fail("background.continuation: resubscribe too many") }
      for entry in entries {
        guard isContinuationSelector(entry) else {
          return fail("background.continuation: resubscribe entry must name canonical UUIDs with positive occurrences")
        }
      }
      resubscribe = entries.count
    } else {
      return fail("background.continuation: resubscribe must be an array")
    }
    if let setup = root["setup"] {
      guard strategy == "native", isContinuationSetup(setup, subscriptions: resubscribe) else {
        return fail("background.continuation: invalid native setup")
      }
    }
    if let value = root["link"] {
      guard strategy == "native", let link = value as? [String: Any], Set(link.keys) == ["mtu"],
            let mtu = link["mtu"] as? [String: Any], Set(mtu.keys) == ["requested", "timeoutMs", "onUnsupported"],
            boundedContinuationInteger(mtu["requested"], minimum: 23, maximum: 517) != nil,
            boundedContinuationInteger(mtu["timeoutMs"], minimum: 1, maximum: 20000) != nil,
            let policy = mtu["onUnsupported"] as? String, ["continue", "fail"].contains(policy) else {
        return fail("background.continuation: invalid native link MTU")
      }
    }
    if let value = root["recording"] {
      guard strategy == "native", let recording = value as? [String: Any],
            Set(recording.keys) == ["id", "maxBytes", "maxRecords"], let id = recording["id"] as? String,
            id.range(of: "^[A-Za-z0-9_-]{1,64}$", options: .regularExpression) != nil,
            boundedContinuationInteger(recording["maxBytes"], minimum: 1048576, maximum: 1073741824) != nil,
            boundedContinuationInteger(recording["maxRecords"], minimum: 1, maximum: 1000000) != nil else {
        return fail("background.continuation: invalid native recording")
      }
    }
    if strategy == "headless-task" {
      guard let name = root["headlessTaskName"] as? String, !name.isEmpty else {
        return fail("background.continuation: headlessTaskName required for headless-task")
      }
    } else if root["headlessTaskName"] != nil {
      return fail("background.continuation: headlessTaskName applies only to headless-task")
    }
    if strategy == "foreground-service" {
      guard let service = root["foregroundService"] as? [String: Any],
            Set(service.keys) == ["notification"],
            let notification = service["notification"] as? [String: Any],
            Set(notification.keys).isSubset(of: ["channelId", "channelName", "title", "body", "icon"]),
            ["channelId", "channelName", "title"].allSatisfy({ Self.isNonEmptyString(notification[$0]) }),
            Self.isMissingOrString(notification["body"]),
            Self.isMissingOrString(notification["icon"]) else {
        return fail("background.continuation: foregroundService notification required for foreground-service")
      }
    } else if root["foregroundService"] != nil {
      return fail("background.continuation: foregroundService applies only to foreground-service")
    }
    return .success(ValidatedContinuation(strategy: strategy, peerId: peerId, resubscribe: resubscribe, recording: root["recording"] != nil))
  }

  /// Counts a malformed persisted declaration once per distinct payload, so
  /// the counter names bad declarations rather than status reads. Survives
  /// process death with the woken process that matters.
  static func countMalformedDeclaration(_ payload: String) {
    let defaults = UserDefaults.standard
    if defaults.string(forKey: continuationMalformedPayloadKey) != payload {
      defaults.set(payload, forKey: continuationMalformedPayloadKey)
      defaults.set(defaults.integer(forKey: continuationMalformedCountKey) + 1, forKey: continuationMalformedCountKey)
    }
  }

  /// The last outcome written by the native executor; null
  /// until one exists, never invented. A record that is not the expected
  /// shape reads as no wake rather than a fabricated one.
  static func readLastWake() -> [String: Any]? {
    guard let text = UserDefaults.standard.string(forKey: continuationLastWakeKey),
          let data = text.data(using: .utf8),
          let wake = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
          wake["observedAtMs"] is NSNumber,
          wake["event"] as? String == "continuation.completed" || wake["event"] as? String == "continuation.failed",
          wake["strategy"] is String,
          wake["peerAddress"] == nil || wake["peerAddress"] is NSNull || wake["peerAddress"] is String,
          wake["code"] == nil || wake["code"] is NSNull || wake["code"] is String,
          wake["reason"] == nil || wake["reason"] is NSNull || wake["reason"] is String else {
      return nil
    }
    return wake
  }

  private static func isNonEmptyString(_ value: Any?) -> Bool {
    guard let text = value as? String else { return false }
    return !text.isEmpty
  }

  /// Optional means absent; explicit null, empty and non-string values are invalid.
  private static func isMissingOrString(_ value: Any?) -> Bool {
    guard let value else { return true }
    return isNonEmptyString(value)
  }

  private static func isMacAddress(_ text: String) -> Bool {
    let parts = text.split(separator: ":", omittingEmptySubsequences: false)
    guard parts.count == 6 else { return false }
    return parts.allSatisfy { $0.count == 2 && $0.allSatisfy({ $0.isHexDigit }) }
  }

  private static func isUuid(_ text: String) -> Bool {
    let parts = text.split(separator: "-", omittingEmptySubsequences: false).map(String.init)
    guard parts.count == 5,
          [8, 4, 4, 4, 12].elementsEqual(parts.map(\.count)) else { return false }
    return parts.joined().allSatisfy({ $0.isHexDigit })
  }

  /// A missing occurrence defaults to 1 on Android; a present one must be a
  /// positive integer (never a boolean, which JSON decodes as a number).
  private static func isPositiveIntOrMissing(_ value: Any?) -> Bool {
    guard let value else { return true }
    guard let number = value as? NSNumber, CFGetTypeID(number) != CFBooleanGetTypeID() else { return false }
    return boundedContinuationInteger(number, minimum: 1, maximum: 9007199254740991) != nil
  }

  private static func boundedContinuationInteger(_ value: Any?, minimum: Int, maximum: Int) -> Int? {
    guard let number = value as? NSNumber, CFGetTypeID(number) != CFBooleanGetTypeID(),
          number.doubleValue >= Double(minimum), number.doubleValue <= Double(maximum),
          number.doubleValue.rounded(.towardZero) == number.doubleValue else { return nil }
    return number.intValue
  }

  private static func isContinuationSelector(_ value: Any) -> Bool {
    guard let selector = value as? [String: Any],
          Set(selector.keys).isSubset(of: ["serviceUuid", "serviceOccurrence", "characteristicUuid", "characteristicOccurrence"]),
          let service = selector["serviceUuid"] as? String, isUuid(service),
          let characteristic = selector["characteristicUuid"] as? String, isUuid(characteristic),
          isPositiveIntOrMissing(selector["serviceOccurrence"]),
          isPositiveIntOrMissing(selector["characteristicOccurrence"]) else { return false }
    return true
  }

  private static func continuationBytes(_ value: Any?, maximum: Int = 512) -> [Int]? {
    guard let values = value as? [Any], !values.isEmpty, values.count <= maximum else { return nil }
    var bytes: [Int] = []
    for value in values {
      guard let byte = boundedContinuationInteger(value, minimum: 0, maximum: 255) else { return nil }
      bytes.append(byte)
    }
    return bytes
  }

  private static func isContinuationSetup(_ value: Any, subscriptions: Int) -> Bool {
    guard let steps = value as? [Any], steps.count <= 16 else { return false }
    var total = 0
    for value in steps {
      guard let step = value as? [String: Any],
            Set(step.keys).isSubset(of: ["selector", "value", "timeoutMs", "response"]),
            let selector = step["selector"], isContinuationSelector(selector),
            continuationBytes(step["value"]) != nil,
            let timeout = boundedContinuationInteger(step["timeoutMs"], minimum: 1, maximum: 20000) else { return false }
      total += timeout
      if total > 60000 { return false }
      if let value = step["response"] {
        guard let reply = value as? [String: Any],
              Set(reply.keys).isSubset(of: ["subscriptionIndex", "prefix", "minLength", "maxLength", "status", "trailing"]),
              boundedContinuationInteger(reply["subscriptionIndex"], minimum: 0, maximum: subscriptions - 1) != nil,
              let prefix = continuationBytes(reply["prefix"]),
              let minimum = boundedContinuationInteger(reply["minLength"], minimum: prefix.count, maximum: 512),
              let maximum = boundedContinuationInteger(reply["maxLength"], minimum: minimum, maximum: 512),
              let status = reply["status"] as? [String: Any], Set(status.keys) == ["offset", "accepted"],
              boundedContinuationInteger(status["offset"], minimum: prefix.count, maximum: minimum - 1) != nil,
              let accepted = continuationBytes(status["accepted"], maximum: 256), Set(accepted).count == accepted.count else { return false }
        if let value = reply["trailing"] {
          guard let trailing = value as? [String: Any], Set(trailing.keys) == ["offset", "accepted"],
                boundedContinuationInteger(trailing["offset"], minimum: minimum, maximum: minimum) != nil,
                maximum == minimum + 1,
                let accepted = continuationBytes(trailing["accepted"], maximum: 256), Set(accepted).count == accepted.count else { return false }
        }
      }
    }
    return true
  }

  // MARK: - Private

  private enum Addressed {
    case success(Entry)
    case failure(String)
  }

  private func entry(_ sessionId: String, operation: String) -> Addressed {
    guard let id = Self.sessionNumber(sessionId) else {
      return .failure(Self.failureJson(
        code: "argument.invalid", domain: "core", operation: operation, detail: "sessionId is not a decimal session id"
      ))
    }
    lock.lock()
    let found = sessions[id]
    let issued = id <= highestIssued
    lock.unlock()
    if let found { return .success(found) }
    return .failure(Self.failureJson(
      code: issued ? "lifecycle.destroyed" : "argument.invalid",
      domain: "core",
      operation: operation,
      detail: issued ? "session \(id) is closed" : "session \(id) was never opened"
    ))
  }

  static func sessionNumber(_ text: String) -> UInt64? {
    guard !text.isEmpty, text.utf8.allSatisfy({ (48...57).contains($0) }),
          text == "0" || !text.hasPrefix("0") else { return nil }
    return UInt64(text)
  }

  private static func positiveUInt32(_ value: Double) -> UInt32? {
    guard let exact = UInt32(exactly: value), exact > 0 else { return nil }
    return exact
  }

  /// `nil` when the dispose envelope reports `released`; otherwise the
  /// failure JSON that keeps the lease.
  static func disposeFailure(_ envelope: String) -> String? {
    guard let data = envelope.data(using: .utf8),
          let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
          let ok = object["ok"] as? Bool else {
      return failureJson(
        code: "protocol.malformed", domain: "core", operation: "rust-core.close-session",
        detail: "unreadable session.dispose envelope"
      )
    }
    if !ok {
      let error = object["error"] as? [String: Any] ?? [:]
      return failureJson(
        code: error["code"] as? String ?? "protocol.malformed",
        domain: error["domain"] as? String ?? "core",
        operation: error["operation"] as? String ?? "session.dispose",
        detail: error["detail"] as? String
      )
    }
    let value = object["value"] as? [String: Any]
    guard value?["state"] as? String == "released" else {
      return failureJson(
        code: "lifecycle.invalid-state", domain: "cleanup", operation: "rust-core.close-session",
        detail: envelope
      )
    }
    return nil
  }

  static func failureJson(_ error: Error, operation: String) -> String {
    if let error = error as? RecordingFailure { return error.json }
    if case let MobileCoreError.Failed(code, domain, failedOperation, detail) = error {
      return failureJson(code: code, domain: domain, operation: failedOperation, detail: detail)
    }
    return failureJson(code: "platform.failure", domain: "platform", operation: operation, detail: "\(error)")
  }

  static func recordingFailureJson(_ error: Error) -> String {
    if let error = error as? RecordingFailure { return error.json }
    if case MobileCoreError.Failed = error { return failureJson(error, operation: "continuation.recording") }
    let reason = "Private recording storage could not be configured"
    return platformFailureJson(error as NSError, operation: "continuation.recording.configure", detail: reason)
  }

  static func platformFailureJson(_ native: NSError, operation: String, detail: String) -> String {
    let record: [String: Any] = ["code": "platform.failure", "domain": "platform", "operation": operation,
      "detail": detail, "platform": ["domain": native.domain, "code": String(native.code), "message": detail, "metadata": [String: Any]()]]
    guard let data = try? JSONSerialization.data(withJSONObject: record, options: [.sortedKeys]) else {
      return failureJson(code: "platform.failure", domain: "platform", operation: operation, detail: detail)
    }
    return String(decoding: data, as: UTF8.self)
  }

  static func failureJson(code: String, domain: String, operation: String, detail: String?) -> String {
    let record: [String: Any] = [
      "code": code, "domain": domain, "operation": operation, "detail": detail ?? NSNull()
    ]
    guard let data = try? JSONSerialization.data(withJSONObject: record, options: [.sortedKeys]),
          let json = String(data: data, encoding: .utf8) else {
      return "{\"code\":\"protocol.malformed\",\"detail\":null,\"domain\":\"core\",\"operation\":\"\(operation)\"}"
    }
    return json
  }
}

private final class InvokeCompletion: MobileInvokeCompletion, @unchecked Sendable {
  private let body: (String) -> Void

  init(_ body: @escaping (String) -> Void) {
    self.body = body
  }

  func complete(envelope: String) {
    body(envelope)
  }
}

/// Restoration identity of the process-owned CoreBluetooth central,
/// derived exactly as the legacy `UnifiedBleProtocolControl` did
/// (`derivedRestorationIdentity`): Info.plist `UnifiedBleProtocolRestorationId`
/// and `UnifiedBleProtocolRestorationGeneration` with the bundle id, through
/// SHA-256 over length-prefixed fields. The restore identifier handed to
/// `CBCentralManager` is `<bundle id>.ubm.<22 base64url chars>`.
enum UnifiedBleRustRestorationIdentity {
  struct Failure: Error, Equatable {
    let detail: String
    var json: String {
      UnifiedBleRustCoreSessions.failureJson(
        code: "platform.failure", domain: "platform", operation: "rust-core.restoration-identity", detail: detail
      )
    }
  }

  static let restorationIdKey = "UnifiedBleProtocolRestorationId"
  static let generationKey = "UnifiedBleProtocolRestorationGeneration"

  static func derive(applicationId: String, restorationId: String, generation: String) -> [String: String] {
    let root = sha256(
      Data("ubm-restoration-v1".utf8) + lengthPrefixed(applicationId) + lengthPrefixed(restorationId)
        + lengthPrefixed(generation)
    )
    func derive(_ label: String) -> String {
      base64Url(sha256(root + Data([0]) + Data(label.utf8)))
    }
    return [
      "applicationId": applicationId,
      "restorationId": restorationId,
      "generation": generation,
      "restoreIdentifier": "\(applicationId).ubm.\(derive("restore").prefix(22))",
      "namespaceValue": "ubm-ns:\(derive("namespace"))",
      "clientId": "ubm-client:\(derive("client"))",
      "hostSessionScope": "ubm-host:\(derive("host"))",
    ]
  }

  static func validToken(_ value: String?, maximumBytes: Int) -> Bool {
    guard let value, !value.isEmpty, value.utf8.count <= maximumBytes else { return false }
    return value.range(of: "^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$", options: .regularExpression) != nil
  }

  /// The configured identity, when the bundle configures a complete one.
  static func configured(bundle: Bundle) -> [String: String]? {
    guard let applicationId = bundle.bundleIdentifier, !applicationId.isEmpty,
          let restorationId = infoString(bundle, restorationIdKey), validToken(restorationId, maximumBytes: 128),
          let generation = infoString(bundle, generationKey), validToken(generation, maximumBytes: 64) else {
      return nil
    }
    let derived = derive(applicationId: applicationId, restorationId: restorationId, generation: generation)
    return derived.values.allSatisfy { !$0.isEmpty } ? derived : nil
  }

  static func configuredRestoreIdentifier(bundle: Bundle = .main) -> String? {
    configured(bundle: bundle)?["restoreIdentifier"]
  }

  /// Legacy `bootstrapRestorationIdentity`: the request must name exactly
  /// the configured restoration id and generation. An empty request `{}`
  /// asks for the configured identity itself (Info.plist-only apps, which
  /// the legacy module served from its init-time authority): the identity
  /// JSON, or `null` when the bundle configures none.
  static func bootstrap(requestJson: String, bundle: Bundle) -> Result<String, Failure> {
    guard let data = requestJson.data(using: .utf8),
          let request = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else {
      return .failure(Failure(detail: "The restoration request must be {restorationId, generation} strings"))
    }
    if request.isEmpty {
      guard let derived = configured(bundle: bundle) else { return .success("null") }
      guard let json = try? JSONSerialization.data(withJSONObject: derived, options: [.sortedKeys]),
            let text = String(data: json, encoding: .utf8) else {
        return .failure(Failure(detail: "The native restoration identity is unavailable"))
      }
      return .success(text)
    }
    guard Set(request.keys) == ["restorationId", "generation"],
          let restorationId = request["restorationId"] as? String,
          let generation = request["generation"] as? String else {
      return .failure(Failure(detail: "The restoration request must be {restorationId, generation} strings"))
    }
    guard let applicationId = bundle.bundleIdentifier, !applicationId.isEmpty,
          validToken(restorationId, maximumBytes: 128), validToken(generation, maximumBytes: 64),
          let configuredId = infoString(bundle, restorationIdKey), configuredId == restorationId,
          let configuredGeneration = infoString(bundle, generationKey), configuredGeneration == generation else {
      return .failure(Failure(detail: "The native restoration configuration does not match the request"))
    }
    let derived = derive(applicationId: applicationId, restorationId: restorationId, generation: generation)
    guard derived["restoreIdentifier"] == configuredRestoreIdentifier(bundle: bundle),
          let json = try? JSONSerialization.data(withJSONObject: derived, options: [.sortedKeys]),
          let text = String(data: json, encoding: .utf8) else {
      return .failure(Failure(detail: "The native restoration identity is unavailable"))
    }
    return .success(text)
  }

  private static func infoString(_ bundle: Bundle, _ key: String) -> String? {
    guard let value = bundle.object(forInfoDictionaryKey: key) as? String, !value.isEmpty else { return nil }
    return value
  }

  private static func lengthPrefixed(_ value: String) -> Data {
    let bytes = Data(value.utf8)
    let length = UInt32(bytes.count)
    return Data([UInt8(length >> 24 & 0xff), UInt8(length >> 16 & 0xff), UInt8(length >> 8 & 0xff), UInt8(length & 0xff)])
      + bytes
  }

  private static func sha256(_ data: Data) -> Data {
    Data(SHA256.hash(data: data))
  }

  private static func base64Url(_ data: Data) -> String {
    data.base64EncodedString()
      .replacingOccurrences(of: "+", with: "-")
      .replacingOccurrences(of: "/", with: "_")
      .replacingOccurrences(of: "=", with: "")
  }
}
