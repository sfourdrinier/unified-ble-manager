package com.sfourdrinier.unifiedblemanager.background;

import java.util.HashSet;
import java.util.Set;
import java.util.function.Supplier;

public final class ConnectedDeviceForegroundServiceLeaseRegistry {
  private final ConnectedDeviceForegroundServiceDriver driver;
  private final Supplier<String> leaseIds;
  private final Set<String> leases = new HashSet<>();
  private ForegroundServiceNotificationConfiguration activeConfiguration;
  private boolean pendingStartCleanup;

  public ConnectedDeviceForegroundServiceLeaseRegistry(
      ConnectedDeviceForegroundServiceDriver driver,
      Supplier<String> leaseIds) {
    this.driver = driver;
    this.leaseIds = leaseIds;
  }

  public synchronized String acquire(String reason) {
    return acquire(reason, driver.notificationConfiguration());
  }

  public synchronized String acquire(String reason, ForegroundServiceNotificationConfiguration configuration) {
    retryPendingStartCleanup();
    if (!leases.isEmpty() && !java.util.Objects.equals(activeConfiguration, configuration)) {
      throw new ForegroundServiceControlException("foregroundServiceConfigurationConflict",
          "An active connected-device lease owns a different notification configuration; release it before replacement.");
    }
    final String leaseId = leaseIds.get();
    if (leases.contains(leaseId)) {
      throw new ForegroundServiceControlException(
          "invalidBackgroundLease",
          "The generated connected-device background lease is not unique.");
    }
    if (leases.isEmpty()) {
      try {
        driver.start(reason, configuration);
      } catch (ForegroundServiceControlException failure) {
        if (failure.cleanupRequired) {
          pendingStartCleanup = true;
          try {
            driver.stop();
            pendingStartCleanup = false;
          } catch (RuntimeException cleanupFailure) {
            failure.addSuppressed(cleanupFailure);
          }
        }
        throw failure;
      }
      activeConfiguration = configuration;
    }
    leases.add(leaseId);
    return leaseId;
  }

  public synchronized void release(String leaseId) {
    if (!leases.contains(leaseId)) {
      throw new ForegroundServiceControlException(
          "invalidBackgroundLease",
          "The connected-device background lease is stale or already released.");
    }
    if (leases.size() == 1) driver.stop();
    leases.remove(leaseId);
    if (leases.isEmpty()) activeConfiguration = null;
  }

  public synchronized void close() {
    if (leases.isEmpty() && !pendingStartCleanup) return;
    driver.stop();
    leases.clear();
    activeConfiguration = null;
    pendingStartCleanup = false;
  }

  public synchronized void update(String leaseId, String title, String body) {
    if (!leases.contains(leaseId)) {
      throw new ForegroundServiceControlException(
          "invalidBackgroundLease",
          "The connected-device background lease is stale or already released.");
    }
    // Validate without replacing the original shared-lease admission identity.
    // The service owns mutable display text, not the registry's pinned order.
    if (activeConfiguration != null) activeConfiguration.withText(title, body);
    driver.update(title, body);
  }

  public synchronized int activeLeaseCount() {
    return leases.size();
  }

  public synchronized boolean hasPendingStartCleanup() { return pendingStartCleanup; }

  public synchronized void retryPendingStartCleanup() {
    if (!pendingStartCleanup) return;
    driver.stop();
    pendingStartCleanup = false;
  }

  public synchronized boolean hasLease(String leaseId) {
    return leases.contains(leaseId);
  }
}
