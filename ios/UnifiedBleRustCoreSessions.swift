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
  private var host: MobileCoreHost?
  private let recordingQueue: DispatchQueue
  private let recordingLock = NSLock()
  private var recordingConfigured = false

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
    var directory = try manager.url(for: .applicationSupportDirectory, in: .userDomainMask,
      appropriateFor: nil, create: true).appendingPathComponent("ubm-continuation", isDirectory: true)
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
    lock.lock()
    defer { lock.unlock() }
    if let host { return host }
    let installed = try installer(self)
    host = installed
    return installed
  }

  private func existingHost() -> MobileCoreHost? {
    lock.lock()
    defer { lock.unlock() }
    return host
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
  /// creation. Merely installing UBM must not allocate a central or prompt:
  /// only an explicitly declared native order opts into this bootstrap.
  public func bootstrapNativeContinuation() -> String? {
    guard let declaration = Self.continuationDeclaration() else { return nil }
    switch Self.validatedContinuation(declaration) {
    case .failure(let error):
      Self.countMalformedDeclaration(declaration)
      return Self.failureJson(code: "argument.invalid", domain: "restoration",
                              operation: "continuation.bootstrap", detail: error.detail)
    case .success(let valid):
      guard valid.strategy == "native" else { return nil }
      #if os(tvOS)
      return Self.failureJson(code: "capability.unsupported", domain: "restoration",
                              operation: "continuation.bootstrap", detail: "tvOS does not provide Bluetooth state restoration")
      #else
      guard Self.productionRadioConfiguration(bundle: .main).restoreIdentifierKey != nil else {
        return Self.failureJson(code: "capability.unsupported", domain: "restoration",
                                operation: "continuation.bootstrap", detail: "native continuation requires a configured restoration identifier")
      }
      if let failure = ensureHost() { return failure }
      let configuration = Self.productionRadioConfiguration(bundle: .main)
      let accessorySetup = OwnedCoreBluetoothProtocolRadioSupport.accessorySetupConfigured(info: Bundle.main.infoDictionary ?? [:])
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

  private static func continuationDeclaration() -> String? {
    UserDefaults.standard.string(forKey: continuationDefaultsKey)
      ?? Bundle.main.object(forInfoDictionaryKey: "UnifiedBleBackgroundContinuation") as? String
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
      lock.lock()
      let installed = host
      lock.unlock()
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
    lock.lock()
    let installed = host
    lock.unlock()
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
