// ios/Owned/OwnedCoreBluetoothProtocolRadioOwner.swift

import Foundation

/** Queue-confined attachment/release state, split from CoreBluetooth for executable race tests. */
final class OwnedCoreBluetoothBorrowerReleaseCoordinator {
  typealias Completion = (NSError?) -> Void

  enum ReleaseAdmission {
    case noBorrower
    case joined
    case start
  }

  private var activeBorrower: ObjectIdentifier?
  private var releasing = false
  private var completions = [Completion]()

  var releaseInProgress: Bool { releasing }

  func attach(_ candidate: AnyObject) -> Bool {
    guard activeBorrower == nil, !releasing else { return false }
    activeBorrower = ObjectIdentifier(candidate)
    return true
  }

  func beginRelease(_ candidate: AnyObject, completion: @escaping Completion) -> ReleaseAdmission {
    guard activeBorrower == ObjectIdentifier(candidate) else { return .noBorrower }
    completions.append(completion)
    guard !releasing else { return .joined }
    releasing = true
    return .start
  }

  func finish(_ error: NSError?) -> [Completion] {
    if error == nil { activeBorrower = nil }
    releasing = false
    let pending = completions
    completions.removeAll()
    return pending
  }
}

extension OwnedCoreBluetoothProtocolRadio {
  /**
   * Claims the sole JavaScript event route for this process-owned radio.
   * A second TurboModule must not steal callbacks from an attached borrower.
   */
  @objc public func attachDelegateIfAvailable(_ candidate: OwnedCoreBluetoothProtocolRadioDelegate) -> Bool {
    queue.sync {
      guard borrowerRelease.attach(candidate) else { return false }
      delegate = candidate
      return true
    }
  }

  /**
   * Releases one borrower without opening an attachment gap during physical
   * cleanup. Concurrent releases join the same terminal cleanup result.
   */
  @objc public func releaseBorrowerIfCurrent(
    _ candidate: OwnedCoreBluetoothProtocolRadioDelegate,
    completion: @escaping (NSError?) -> Void
  ) {
    queue.async {
      switch self.borrowerRelease.beginRelease(candidate, completion: completion) {
      case .noBorrower:
        completion(nil)
        return
      case .joined:
        return
      case .start:
        break
      }
      self.releaseProtocolClient { error in
        if error == nil, let current = self.delegate, current === candidate {
          self.delegate = nil
        }
        let completions = self.borrowerRelease.finish(error)
        if error != nil {
          self.scheduleBorrowerReleaseRetry(candidate)
        }
        for pendingCompletion in completions {
          pendingCompletion(error)
        }
      }
    }
  }

  private func scheduleBorrowerReleaseRetry(_ candidate: OwnedCoreBluetoothProtocolRadioDelegate) {
    guard !borrowerReleaseRetryScheduled else { return }
    borrowerReleaseRetryScheduled = true
    queue.asyncAfter(deadline: .now() + .seconds(1)) {
      self.borrowerReleaseRetryScheduled = false
      self.releaseBorrowerIfCurrent(candidate) { _ in }
    }
  }
}

/**
 * Process lifetime registry for CoreBluetooth centrals selected by immutable
 * native configuration. React Native may construct its TurboModule more than
 * once; those modules borrow the same radio rather than registering duplicate
 * CoreBluetooth restoration identifiers.
 */
@objc(OwnedCoreBluetoothProtocolRadioOwner)
public final class OwnedCoreBluetoothProtocolRadioOwner: NSObject {
  private struct Configuration: Hashable {
    let restoreIdentifierKey: String?
    let showPowerAlert: Bool?
  }

  private static let lock = NSLock()
  private static var radios = [Configuration: OwnedCoreBluetoothProtocolRadio]()

  @objc(acquireWithRestoreIdentifierKey:showPowerAlert:)
  public static func acquire(
    restoreIdentifierKey: String?,
    showPowerAlert: NSNumber?
  ) -> OwnedCoreBluetoothProtocolRadio {
    let configuration = Configuration(
      restoreIdentifierKey: restoreIdentifierKey,
      showPowerAlert: showPowerAlert?.boolValue
    )
    lock.lock()
    defer { lock.unlock() }
    if let existing = radios[configuration] { return existing }
    let created = OwnedCoreBluetoothProtocolRadio(
      restoreIdentifierKey: restoreIdentifierKey,
      showPowerAlert: showPowerAlert
    )
    radios[configuration] = created
    return created
  }

  /**
   * A borrower releases its own radio work only when it still owns callback
   * delivery. The process-owned central remains available for restoration and
   * a replacement TurboModule.
   */
  @objc(releaseBorrowerWithRadio:delegate:completion:)
  public static func releaseBorrower(
    radio: OwnedCoreBluetoothProtocolRadio,
    delegate: OwnedCoreBluetoothProtocolRadioDelegate,
    completion: @escaping (NSError?) -> Void
  ) {
    radio.releaseBorrowerIfCurrent(delegate, completion: completion)
  }
}
