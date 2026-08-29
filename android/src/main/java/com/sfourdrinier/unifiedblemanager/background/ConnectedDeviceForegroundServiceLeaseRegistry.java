package com.sfourdrinier.unifiedblemanager.background;

import java.util.HashSet;
import java.util.Set;
import java.util.function.Supplier;

public final class ConnectedDeviceForegroundServiceLeaseRegistry {
  private final ConnectedDeviceForegroundServiceDriver driver;
  private final Supplier<String> leaseIds;
  private final Set<String> leases = new HashSet<>();

  public ConnectedDeviceForegroundServiceLeaseRegistry(
      ConnectedDeviceForegroundServiceDriver driver,
      Supplier<String> leaseIds) {
    this.driver = driver;
    this.leaseIds = leaseIds;
  }

  public synchronized String acquire(String reason) {
    final String leaseId = leaseIds.get();
    if (leases.contains(leaseId)) {
      throw new ForegroundServiceControlException(
          "invalidBackgroundLease",
          "The generated connected-device background lease is not unique.");
    }
    if (leases.isEmpty()) driver.start(reason);
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
  }

  public synchronized void close() {
    if (leases.isEmpty()) return;
    driver.stop();
    leases.clear();
  }

  public synchronized void update(String leaseId, String title, String body) {
    if (!leases.contains(leaseId)) {
      throw new ForegroundServiceControlException(
          "invalidBackgroundLease",
          "The connected-device background lease is stale or already released.");
    }
    driver.update(title, body);
  }

  public synchronized int activeLeaseCount() {
    return leases.size();
  }

  public synchronized boolean hasLease(String leaseId) {
    return leases.contains(leaseId);
  }
}
