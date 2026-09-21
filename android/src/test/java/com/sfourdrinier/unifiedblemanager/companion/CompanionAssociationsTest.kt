// android/src/test/java/com/sfourdrinier/unifiedblemanager/companion/CompanionAssociationsTest.kt

package com.sfourdrinier.unifiedblemanager.companion

import android.companion.AssociationInfo
import android.net.MacAddress
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import org.mockito.Mockito.mock
import org.mockito.Mockito.`when`

/**
 * Finding 236: associating an already-associated device must not silently
 * create a duplicate. The lookup both associate routes share matches an
 * existing association by exact display name.
 */
class CompanionAssociationsTest {
  private fun summary(id: Int, mac: String?, name: String?) =
    CompanionAssociations.Summary(id, mac, name)

  @Test
  fun exactDisplayNameFindsTheExistingAssociation() {
    val associations = listOf(
      summary(3, "11:22:33:44:55:66", "S39 4BEA LE"),
      summary(4, "A0:9E:1A:E9:B9:3D", "Polar H10 E9B93D29")
    )
    val found = CompanionAssociations.findByDisplayName(associations, "Polar H10 E9B93D29")
    assertEquals(4, found!!.id)
    assertEquals("A0:9E:1A:E9:B9:3D", found.macAddress)
  }

  @Test
  fun unknownNameMatchesNothing() {
    val associations = listOf(summary(4, "A0:9E:1A:E9:B9:3D", "Polar H10 E9B93D29"))
    assertNull(CompanionAssociations.findByDisplayName(associations, "Polar Verity Sense"))
  }

  @Test
  fun prefixAloneNeverMatches() {
    // The request quotes the name for a full-match; the lookup keeps the
    // same semantics so a short name cannot claim another device's record.
    val associations = listOf(summary(4, "A0:9E:1A:E9:B9:3D", "Polar H10 E9B93D29"))
    assertNull(CompanionAssociations.findByDisplayName(associations, "Polar H10"))
  }

  @Test
  fun nullNameAndNullListNeverMatch() {
    val associations = listOf(summary(4, "A0:9E:1A:E9:B9:3D", "Polar H10 E9B93D29"))
    assertNull(CompanionAssociations.findByDisplayName(associations, null))
    assertNull(CompanionAssociations.findByDisplayName(null, "Polar H10 E9B93D29"))
    assertNull(CompanionAssociations.findByDisplayName(associations, ""))
  }

  @Test
  fun nullDisplayNamesAreSkipped() {
    val associations = listOf(summary(4, "A0:9E:1A:E9:B9:3D", null))
    assertNull(CompanionAssociations.findByDisplayName(associations, "Polar H10 E9B93D29"))
  }

  @Test
  fun summarizeMapsThePlatformRecords() {
    val info = mock(AssociationInfo::class.java)
    `when`(info.id).thenReturn(5)
    val mac = mock(MacAddress::class.java)
    `when`(mac.toString()).thenReturn("A0:9E:1A:E9:B9:3D")
    `when`(info.deviceMacAddress).thenReturn(mac)
    `when`(info.displayName).thenReturn("Polar H10 E9B93D29")

    val summaries = CompanionAssociations.summarize(listOf(info, null))
    assertEquals(1, summaries.size)
    assertEquals(5, summaries[0].id)
    assertEquals("A0:9E:1A:E9:B9:3D", summaries[0].macAddress)
    assertEquals("Polar H10 E9B93D29", summaries[0].displayName)
  }

  @Test
  fun summarizeOfNullIsEmpty() {
    assertEquals(0, CompanionAssociations.summarize(null).size)
  }
}
