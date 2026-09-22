// android/src/main/java/com/sfourdrinier/unifiedblemanager/companion/CompanionAssociations.java

package com.sfourdrinier.unifiedblemanager.companion;

import android.companion.AssociationInfo;
import android.net.MacAddress;
import java.util.ArrayList;
import java.util.List;

/**
 * Companion Device Manager association lookup shared by both associate routes
 * (finding 236): the Rust-route chooser
 * ({@code ReactCompanionChooser}) and the legacy protocol-control module
 * ({@code UnifiedBleProtocolControlModule}).
 *
 * <p>Companion Device Manager allows several associations for one device, so
 * associating an already-associated strap silently accumulates duplicates —
 * and the OS then delivers one presence appearance per association, running
 * the wake once per duplicate. Both routes check for an existing association
 * for the requested device before launching the system UI.
 *
 * <p>Kept dependency-free (no native library load, no React types) so plain
 * JVM unit tests can pin it. Everything here is a pure function over the
 * summaries the caller mapped from {@code CompanionDeviceManager}.
 */
public final class CompanionAssociations {
  private CompanionAssociations() {}

  /** One of this app's associations: the fields the dedup decision needs. */
  public static final class Summary {
    public final int id;
    public final String macAddress;
    public final String displayName;

    public Summary(int id, String macAddress, String displayName) {
      this.id = id;
      this.macAddress = macAddress;
      this.displayName = displayName;
    }
  }

  /** Maps the platform records to summaries; a null platform list is no associations. */
  public static List<Summary> summarize(List<AssociationInfo> infos) {
    final List<Summary> summaries = new ArrayList<>();
    if (infos == null) return summaries;
    for (AssociationInfo info : infos) {
      if (info == null) continue;
      final MacAddress mac = info.getDeviceMacAddress();
      final CharSequence displayName = info.getDisplayName();
      summaries.add(
          new Summary(
              info.getId(),
              mac == null ? null : mac.toString(),
              displayName == null ? null : displayName.toString()));
    }
    return summaries;
  }

  /**
   * The existing association whose display name is exactly the requested
   * device name, or null when no association names that device. The match
   * is exact (the association request itself quotes the name for a
   * full-match), so a strap never collides with a differently-named peer.
   * A null or empty name cannot identify a device: it never matches.
   */
  public static Summary findByDisplayName(List<Summary> associations, String name) {
    if (associations == null || name == null || name.isEmpty()) return null;
    for (Summary association : associations) {
      if (association != null && name.equals(association.displayName)) return association;
    }
    return null;
  }
}
