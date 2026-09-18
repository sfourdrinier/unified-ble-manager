// android/src/test/java/com/sfourdrinier/unifiedblemanager/rustcore/OwnedRadioPortMappingTest.kt

package com.sfourdrinier.unifiedblemanager.rustcore

import com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidSecurityState
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertThrows
import org.junit.Test

class OwnedRadioPortMappingTest {
  @Test
  fun bondStatesUseTheFrozenVocabularyAndUnanswerableFactsAreUnsupported() {
    assertEquals(
      SecurityFacts("not-bonded", "unsupported", "unsupported", "unsupported", true),
      OwnedRadioPort.securityFacts(OwnedAndroidSecurityState("notBonded", true))
    )
    assertEquals("bonded", OwnedRadioPort.securityFacts(OwnedAndroidSecurityState("bonded", null)).bond)
    assertEquals("bonding", OwnedRadioPort.securityFacts(OwnedAndroidSecurityState("bonding", true)).bond)
    assertEquals("unknown", OwnedRadioPort.securityFacts(OwnedAndroidSecurityState("weird", null)).bond)
    assertNull(OwnedRadioPort.securityFacts(OwnedAndroidSecurityState("bonded", null)).pairingPossible)
  }

  @Test
  fun phyNamesRoundTripBetweenWireAndDriver() {
    listOf("le-1m" to "le1m", "le-2m" to "le2m", "le-coded" to "leCoded").forEach { (wire, driver) ->
      assertEquals(driver, OwnedRadioPort.driverPhy(wire))
      assertEquals(wire, OwnedRadioPort.wirePhy(driver))
    }
    assertNull(OwnedRadioPort.driverPhy(null))
    assertEquals(
      RadioFailureKind.UNSUPPORTED,
      assertThrows(RadioPortFailure::class.java) { OwnedRadioPort.driverPhy("le-3m") }.kind
    )
    assertEquals(
      RadioFailureKind.PLATFORM,
      assertThrows(RadioPortFailure::class.java) { OwnedRadioPort.wirePhy("bogus") }.kind
    )
  }
}
