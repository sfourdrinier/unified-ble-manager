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

  typealias Installer = (MobileWakeSink) throws -> MobileCoreHost

  private struct Entry {
    let session: MobileCoreSession
    weak var owner: NSObject?
    let onWake: (String) -> Void
  }

  private let installer: Installer
  private let lock = NSLock()
  private var host: MobileCoreHost?
  private var sessions = [UInt64: Entry]()
  private var highestIssued: UInt64 = 0
  /// Wakes that raced ahead of their session's registration (Rust admitted
  /// the session, the module has not stored it yet). Bounded by the number
  /// of concurrent opens.
  private var earlyWakes = Set<UInt64>()

  init(installer: @escaping Installer) {
    self.installer = installer
    super.init()
  }

  // MARK: - Host

  /// Installs the process host once. Called at module init (restoration
  /// needs the restoring CoreBluetooth central early, as legacy did) and
  /// again by every `openSession`, which reports an install failure.
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
    lock.lock()
    defer { lock.unlock() }
    if let host { return host }
    let installed = try installer(self)
    host = installed
    return installed
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
    let configuration = productionRadioConfiguration(bundle: bundle)
    let radio = OwnedCoreBluetoothProtocolRadioOwner.acquire(
      restoreIdentifierKey: configuration.restoreIdentifierKey,
      showPowerAlert: configuration.showPowerAlert
    )
    // Finding 179: `willRestoreState` only lands on a central created with
    // the restore identifier, so a restoring app keeps legacy central timing
    // (and the platform prompt) at startup; without restoration the central
    // waits for first explicit need and the prompt appears on request.
    if OwnedCoreBluetoothProtocolRadioSupport.restorationConfigured(
      restoreIdentifierKey: configuration.restoreIdentifierKey
    ) {
      _ = radio.ensureCentral()
    }
    let adapter = UnifiedBleRustRadioAdapter(driver: radio)
    guard radio.attachDelegateIfAvailable(adapter) else {
      throw MobileCoreError.Failed(
        code: "lifecycle.invalid-state",
        domain: "core",
        operation: "rust-core.host.install",
        detail: "another native module already receives the process CoreBluetooth radio's callbacks"
      )
    }
    do {
      let host = try mobileHostInstall(
        radio: adapter,
        wake: wake,
        platform: "apple",
        owner: "unified-ble-manager.react-native.apple",
        adapterLabel: "corebluetooth"
      )
      adapter.bind(sink: host)
      return host
    } catch {
      radio.releaseBorrowerIfCurrent(adapter) { releaseError in
        if let releaseError {
          NSLog("[UnifiedBleRustCoreSessions] releasing the radio after a failed install failed: %@", releaseError)
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
    let session: MobileCoreSession
    do {
      session = try installedHost().openSession(owner: owner, expectedWireRevision: expectedWireRevision)
    } catch {
      return completion(nil, Self.failureJson(error, operation: "rust-core.open-session"))
    }
    let id = session.sessionId()
    lock.lock()
    sessions[id] = Entry(session: session, owner: ownerToken, onWake: onWake)
    highestIssued = max(highestIssued, id)
    let wokeEarly = earlyWakes.remove(id) != nil
    lock.unlock()
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
    switch entry(sessionId, operation: "rust-core.invoke") {
    case let .failure(failure):
      completion(nil, failure)
    case let .success(entry):
      entry.session.invoke(op: op, argsJson: argsJson, completion: InvokeCompletion { envelope in
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
    lock.unlock()
    guard let held else { return completion(nil) }
    held.session.invoke(op: "session.dispose", argsJson: "{}", completion: InvokeCompletion { envelope in
      if let failure = Self.disposeFailure(envelope) {
        return completion(failure)
      }
      self.lock.lock()
      self.sessions.removeValue(forKey: id)
      self.lock.unlock()
      completion(nil)
    })
  }

  /// Module teardown (React reload): the JS owner of these sessions is gone.
  @objc(closeSessionsOwnedBy:)
  public func closeSessions(ownedBy ownerToken: NSObject) {
    lock.lock()
    let owned = sessions.filter { $0.value.owner === ownerToken || $0.value.owner == nil }.map(\.key)
    lock.unlock()
    for id in owned {
      closeSession(String(id)) { failure in
        if let failure {
          NSLog("[UnifiedBleRustCoreSessions] disposing session %llu at module teardown failed: %@", id, failure)
        }
      }
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

  // MARK: - Background continuation (BGS4; iOS wake execution deferred to rc.1)

  /// Persists the declared standing order so a future wake can execute it.
  /// The declaration is validated with the same rules the Android wake
  /// enforces: a malformed order is refused with no effect, never stored
  /// verbatim. iOS wake execution is deferred, so the status reports the
  /// validated order with the same "not implemented in this release" words
  /// the Android path uses, and the claim answers `capability.unsupported`.
  public func declareBackgroundContinuation(_ declarationJson: String, completion: (String?, String?) -> Void) {
    guard !declarationJson.isEmpty, declarationJson.utf8.count <= Self.maxContinuationJson else {
      completion(nil, Self.failureJson(
        code: "argument.invalid", domain: "restoration", operation: "continuation.declare",
        detail: "declaration must be 1..\(Self.maxContinuationJson) bytes"
      ))
      return
    }
    switch Self.validatedContinuation(declarationJson) {
    case .success:
      UserDefaults.standard.set(declarationJson, forKey: Self.continuationDefaultsKey)
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
  /// non-`record-only` strategy carries the deferred-execution disclaimer in
  /// `detail` with the same words the Android path uses; `lastWake` is null
  /// until a wake executes one, never invented.
  public func continuationStatus(_ completion: (String?, String?) -> Void) {
    var strategy = "record-only"
    var peerId: String?
    var resubscribe = 0
    if let stored = UserDefaults.standard.string(forKey: Self.continuationDefaultsKey) {
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
      "lastWake": Self.readLastWake() as Any? ?? NSNull()
    ]
    if strategy != "record-only" {
      status["detail"] = "\(strategy) continuation is not implemented in this release"
    }
    guard let data = try? JSONSerialization.data(withJSONObject: status, options: [.sortedKeys]),
          let text = String(data: data, encoding: .utf8)
    else {
      completion(nil, Self.failureJson(
        code: "platform.failure", domain: "platform", operation: "continuation.status",
        detail: "status encoding failed"
      ))
      return
    }
    completion(text, nil)
  }

  public func prepareContinuationClaim(maxItems _: Double, maxBytes _: Double, completion: (String?, String?) -> Void) {
    completion(nil, Self.failureJson(
      code: "capability.unsupported", domain: "restoration", operation: "continuation.claim",
      detail: "native reconnect plus resubscribe from willRestoreState is not implemented in this release"
    ))
  }

  public func acknowledgeContinuationClaim(_ claimToken: String, completion: (String?, String?) -> Void) {
    completion(nil, Self.failureJson(
      code: "capability.unsupported", domain: "restoration", operation: "continuation.claim",
      detail: "native reconnect plus resubscribe from willRestoreState is not implemented in this release"
    ))
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
  }

  struct ContinuationValidationError: Error {
    let detail: String
  }

  /// Validates a declaration with the same rules the Android wake enforces:
  /// the exact key set the binding persists, a known strategy, a MAC peer,
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
    let topKeys: Set<String> = ["onAppearance", "peerId", "resubscribe", "headlessTaskName", "foregroundService"]
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
    } else if let text = root["peerId"] as? String, isMacAddress(text) {
      peerId = text.uppercased()
    } else {
      return fail("background.continuation: peerId must be a MAC address")
    }
    let resubscribe: Int
    if root["resubscribe"] == nil {
      resubscribe = 0
    } else if let entries = root["resubscribe"] as? [Any] {
      if entries.count > 64 { return fail("background.continuation: resubscribe too many") }
      for entry in entries {
        guard let selector = entry as? [String: Any],
              Set(selector.keys) == ["serviceUuid", "serviceOccurrence", "characteristicUuid", "characteristicOccurrence"],
              let service = selector["serviceUuid"] as? String, isUuid(service),
              let characteristic = selector["characteristicUuid"] as? String, isUuid(characteristic),
              isPositiveIntOrMissing(selector["serviceOccurrence"]),
              isPositiveIntOrMissing(selector["characteristicOccurrence"]) else {
          return fail("background.continuation: resubscribe entry must name canonical UUIDs with positive occurrences")
        }
      }
      resubscribe = entries.count
    } else {
      return fail("background.continuation: resubscribe must be an array")
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
    return .success(ValidatedContinuation(strategy: strategy, peerId: peerId, resubscribe: resubscribe))
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

  /// The last wake outcome a future wake persists (rc.1 writes it); null
  /// until one exists, never invented. A record that is not the expected
  /// shape reads as no wake rather than a fabricated one.
  static func readLastWake() -> [String: Any]? {
    guard let text = UserDefaults.standard.string(forKey: continuationLastWakeKey),
          let data = text.data(using: .utf8),
          let wake = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
          wake["observedAtMs"] is NSNumber,
          wake["event"] as? String == "continuation.completed" || wake["event"] as? String == "continuation.failed",
          wake["strategy"] is String,
          wake["peerAddress"] == nil || wake["peerAddress"] is String,
          wake["code"] == nil || wake["code"] is String,
          wake["reason"] == nil || wake["reason"] is String else {
      return nil
    }
    return wake
  }

  private static func isNonEmptyString(_ value: Any?) -> Bool {
    guard let text = value as? String else { return false }
    return !text.isEmpty
  }

  /// A missing notification field is absent on Android too: the binding may
  /// persist an explicit null, which reads as missing rather than malformed.
  private static func isMissingOrString(_ value: Any?) -> Bool {
    guard let value else { return true }
    return value is NSNull || value is String
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
    return number.int64Value >= 1 && number.doubleValue == Double(number.int64Value)
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
    if case let MobileCoreError.Failed(code, domain, failedOperation, detail) = error {
      return failureJson(code: code, domain: domain, operation: failedOperation, detail: detail)
    }
    return failureJson(code: "platform.failure", domain: "platform", operation: operation, detail: "\(error)")
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
