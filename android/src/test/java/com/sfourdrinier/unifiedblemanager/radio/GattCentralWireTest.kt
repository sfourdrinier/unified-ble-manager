// android/src/test/java/com/sfourdrinier/unifiedblemanager/radio/GattCentralWireTest.kt

package com.sfourdrinier.unifiedblemanager.radio

import org.junit.Assert.assertEquals
import org.junit.Assert.assertThrows
import org.junit.Test

class GattCentralWireTest {
  @Test
  fun scopedLifecycleLinesHaveExactArityAndGeneration() {
    assertEquals(
      "link.established.scoped|peer-a|generation-a",
      GattCentralWire.linkEstablishedScoped("peer-a", "generation-a")
    )
    assertEquals(
      "link.released.scoped|peer-a|generation-a",
      GattCentralWire.linkReleasedScoped("peer-a", "generation-a")
    )
    assertEquals(
      "peer.loss.scoped|peer-a|generation-a|42",
      GattCentralWire.peerLossScoped("peer-a", "generation-a", 42)
    )
  }

  @Test
  fun scopedLifecycleBuildersRejectEmptyOrDelimitedGeneration() {
    assertThrows(IllegalArgumentException::class.java) {
      GattCentralWire.linkEstablishedScoped("peer-a", "")
    }
    assertThrows(IllegalArgumentException::class.java) {
      GattCentralWire.linkReleasedScoped("peer-a", "generation|bad")
    }
    assertThrows(IllegalArgumentException::class.java) {
      GattCentralWire.peerLossScoped("peer-a", "", 42)
    }
  }
}
