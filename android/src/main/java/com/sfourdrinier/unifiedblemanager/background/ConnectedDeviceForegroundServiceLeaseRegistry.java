package com.sfourdrinier.unifiedblemanager.background;

import java.util.ArrayList;
import java.util.Collections;
import java.util.HashMap;
import java.util.HashSet;
import java.util.IdentityHashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.Set;
import java.util.function.Supplier;

public final class ConnectedDeviceForegroundServiceLeaseRegistry {
  private final ConnectedDeviceForegroundServiceDriver driver;
  private final Supplier<String> leaseIds;
  // Driver calls are serialized, but never hold stateLock: fencing must not wait for Android.
  private final Object driverLock = new Object();
  private final Object stateLock = new Object();
  private final Map<String, Object> leases = new HashMap<>();
  private final Map<Object, OwnerState> owners = new IdentityHashMap<>();
  private final Object legacyOwner = new Object();
  private ForegroundServiceNotificationConfiguration activeConfiguration;
  private Object pendingStartOwner;
  private String pendingStartId;

  private static final class OwnerState {
    String retirement;
    int inFlight;
    String startInFlightId;
    Object startInFlightToken;
    final List<Throwable> failures = new ArrayList<>();
    final Map<String, List<Throwable>> leaseFailures = new HashMap<>();
  }

  public static final class OwnerSnapshot {
    public final Set<String> leases;
    public final int inFlight;
    public final boolean pendingStartCleanup;
    public final String pendingStartId;
    public final List<Throwable> failures;
    private OwnerSnapshot(Set<String> leases, int inFlight, String pendingStartId, boolean pendingStartCleanup, List<Throwable> failures) {
      this.leases = Collections.unmodifiableSet(leases);
      this.inFlight = inFlight;
      this.pendingStartId = pendingStartId;
      this.pendingStartCleanup = pendingStartCleanup;
      this.failures = Collections.unmodifiableList(failures);
    }
  }

  public ConnectedDeviceForegroundServiceLeaseRegistry(ConnectedDeviceForegroundServiceDriver driver, Supplier<String> leaseIds) {
    this.driver = driver;
    this.leaseIds = leaseIds;
  }

  private OwnerState state(Object owner) {
    return owners.computeIfAbsent(Objects.requireNonNull(owner, "owner"), ignored -> new OwnerState());
  }

  public boolean fenceOwner(Object owner, String retirement) {
    Objects.requireNonNull(retirement, "retirement");
    synchronized (stateLock) {
      OwnerState state = state(owner);
      if (state.retirement != null && !state.retirement.equals(retirement)) return false;
      state.retirement = retirement;
      return true;
    }
  }

  public OwnerSnapshot snapshotOwner(Object owner) {
    synchronized (stateLock) {
      OwnerState state = state(owner);
      Set<String> owned = new HashSet<>();
      leases.forEach((id, leaseOwner) -> { if (leaseOwner == owner) owned.add(id); });
      List<Throwable> failures = new ArrayList<>(state.failures);
      state.leaseFailures.values().forEach(failures::addAll);
      boolean cleanup = pendingStartOwner == owner;
      return new OwnerSnapshot(owned, state.inFlight, cleanup ? pendingStartId : state.startInFlightId, cleanup, failures);
    }
  }

  private void requireFence(Object owner, String retirement) {
    OwnerState state = owners.get(owner);
    if (state == null || state.retirement == null || !state.retirement.equals(retirement)) {
      throw new ForegroundServiceControlException("invalidBackgroundOwner", "The background owner retirement fence does not match.");
    }
  }

  /** Retry only the named retired owner. In-flight admission must finish first. */
  public void retryOwner(Object owner, String retirement) {
    synchronized (stateLock) {
      requireFence(owner, retirement);
      if (state(owner).inFlight != 0) return;
      state(owner).inFlight++;
    }
    try {
      synchronized (driverLock) {
        try {
          synchronized (stateLock) {
            if (pendingStartOwner != owner && leases.values().stream().noneMatch(value -> value == owner)) return;
            boolean otherLease = leases.values().stream().anyMatch(value -> value != owner);
            if (otherLease) {
              leases.entrySet().removeIf(entry -> entry.getValue() == owner);
              state(owner).failures.clear();
              state(owner).leaseFailures.clear();
              return;
            }
          }
          driver.stop();
          synchronized (stateLock) {
            leases.entrySet().removeIf(entry -> entry.getValue() == owner);
            if (pendingStartOwner == owner) { pendingStartOwner = null; pendingStartId = null; }
            if (leases.isEmpty()) activeConfiguration = null;
            state(owner).failures.clear();
            state(owner).leaseFailures.clear();
          }
        } catch (RuntimeException failure) {
          synchronized (stateLock) { state(owner).failures.add(failure); }
          throw failure;
        }
      }
    } finally {
      synchronized (stateLock) { state(owner).inFlight--; }
    }
  }

  public String acquire(String reason) { return acquireOwned(legacyOwner, reason); }
  public String acquire(String reason, ForegroundServiceNotificationConfiguration configuration) { return acquireOwned(legacyOwner, reason, configuration); }
  public String acquireOwned(Object owner, String reason) { return acquireInternal(owner, reason, null, false); }
  public String acquireOwned(Object owner, String reason, ForegroundServiceNotificationConfiguration configuration) { return acquireInternal(owner, reason, configuration, true); }

  private String acquireInternal(Object owner, String reason, ForegroundServiceNotificationConfiguration configuration, boolean explicit) {
    synchronized (stateLock) {
      OwnerState state = state(owner);
      if (state.retirement != null) throw new ForegroundServiceControlException("invalidBackgroundOwner", "The background owner is retired.");
      state.inFlight++;
    }
    Object startToken = null;
    try {
      synchronized (driverLock) {
        boolean retryStart;
        synchronized (stateLock) {
          if (pendingStartOwner != null && pendingStartOwner != owner) throw new ForegroundServiceControlException("foregroundServiceCleanupPending", "A previous accepted service start requires owner cleanup.");
          retryStart = pendingStartOwner == owner;
        }
        if (retryStart) {
          try { driver.stop(); }
          catch (RuntimeException failure) { synchronized (stateLock) { state(owner).failures.add(failure); } throw failure; }
          synchronized (stateLock) { pendingStartOwner = null; pendingStartId = null; state(owner).failures.clear(); }
        }
        ForegroundServiceNotificationConfiguration selected = explicit ? configuration : driver.notificationConfiguration();
        boolean start;
        synchronized (stateLock) {
          if (!leases.isEmpty() && !Objects.equals(activeConfiguration, selected)) throw new ForegroundServiceControlException("foregroundServiceConfigurationConflict", "An active connected-device lease owns a different notification configuration; release it before replacement.");
        }
        String leaseId = Objects.requireNonNull(leaseIds.get(), "leaseId");
        synchronized (stateLock) {
          if (leases.containsKey(leaseId)) throw new ForegroundServiceControlException("invalidBackgroundLease", "The generated connected-device background lease is not unique.");
          start = leases.isEmpty();
        }
        if (start) {
          startToken = new Object();
          synchronized (stateLock) { state(owner).startInFlightId = leaseId; state(owner).startInFlightToken = startToken; }
          try { driver.start(reason, selected); }
          catch (ForegroundServiceControlException failure) {
            if (failure.cleanupRequired) {
              synchronized (stateLock) { pendingStartOwner = owner; pendingStartId = leaseId; }
              try {
                driver.stop();
                synchronized (stateLock) { pendingStartOwner = null; pendingStartId = null; }
              } catch (RuntimeException cleanupFailure) {
                synchronized (stateLock) { state(owner).failures.add(cleanupFailure); }
                failure.addSuppressed(cleanupFailure);
              }
            }
            throw failure;
          }
        }
        synchronized (stateLock) {
          if (start) activeConfiguration = selected;
          leases.put(leaseId, owner);
          if (start && state(owner).startInFlightToken == startToken) {
            state(owner).startInFlightId = null;
            state(owner).startInFlightToken = null;
          }
        }
        return leaseId;
      }
    } finally {
      synchronized (stateLock) {
        if (startToken != null && state(owner).startInFlightToken == startToken) {
          state(owner).startInFlightId = null;
          state(owner).startInFlightToken = null;
        }
        state(owner).inFlight--;
      }
    }
  }

  public void release(String leaseId) { releaseOwned(legacyOwner, leaseId); }
  public void releaseOwned(Object owner, String leaseId) {
    beginLeaseWork(owner, leaseId);
    try {
      synchronized (driverLock) {
        boolean last;
        synchronized (stateLock) {
          requireLease(owner, leaseId);
          last = leases.size() == 1;
        }
        try { if (last) driver.stop(); }
        catch (RuntimeException failure) { synchronized (stateLock) { state(owner).leaseFailures.computeIfAbsent(leaseId, ignored -> new ArrayList<>()).add(failure); } throw failure; }
        synchronized (stateLock) { leases.remove(leaseId); if (leases.isEmpty()) activeConfiguration = null; state(owner).leaseFailures.remove(leaseId); }
      }
    } finally {
      synchronized (stateLock) { state(owner).inFlight--; }
    }
  }

  public void close() {
    synchronized (driverLock) {
      synchronized (stateLock) { if (leases.isEmpty() && pendingStartOwner == null) return; }
      driver.stop();
      synchronized (stateLock) { leases.clear(); activeConfiguration = null; pendingStartOwner = null; pendingStartId = null; owners.values().forEach(state -> { state.failures.clear(); state.leaseFailures.clear(); }); }
    }
  }

  public void update(String leaseId, String title, String body) { updateOwned(legacyOwner, leaseId, title, body); }
  private void requireLease(Object owner, String leaseId) {
    if (owner == null || leases.get(leaseId) != owner) throw new ForegroundServiceControlException("invalidBackgroundLease", "The connected-device background lease is stale or already released.");
  }
  private void beginLeaseWork(Object owner, String leaseId) {
    synchronized (stateLock) {
      requireLease(owner, leaseId);
      if (state(owner).retirement != null) throw new ForegroundServiceControlException("invalidBackgroundLease", "The connected-device background owner is retired.");
      state(owner).inFlight++;
    }
  }
  public void updateOwned(Object owner, String leaseId, String title, String body) {
    beginLeaseWork(owner, leaseId);
    try {
      synchronized (driverLock) {
        synchronized (stateLock) {
          requireLease(owner, leaseId);
          if (activeConfiguration != null) activeConfiguration.withText(title, body);
        }
        driver.update(title, body);
      }
    } finally {
      synchronized (stateLock) { state(owner).inFlight--; }
    }
  }
  public int activeLeaseCount() { synchronized (stateLock) { return leases.size(); } }
  public boolean hasPendingStartCleanup() { synchronized (stateLock) { return pendingStartOwner != null; } }
  public void retryPendingStartCleanup() {
    synchronized (driverLock) {
      synchronized (stateLock) {
        if (pendingStartOwner != legacyOwner) return;
      }
      driver.stop();
      synchronized (stateLock) { pendingStartOwner = null; pendingStartId = null; state(legacyOwner).failures.clear(); }
    }
  }
  public boolean hasLease(String leaseId) { synchronized (stateLock) { return leases.containsKey(leaseId); } }
}
