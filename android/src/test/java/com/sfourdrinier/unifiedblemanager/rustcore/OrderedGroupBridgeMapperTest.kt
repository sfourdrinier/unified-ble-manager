// android/src/test/java/com/sfourdrinier/unifiedblemanager/rustcore/OrderedGroupBridgeMapperTest.kt
package com.sfourdrinier.unifiedblemanager.rustcore

import org.junit.Assert.*
import org.junit.Test

class OrderedGroupBridgeMapperTest {
  @Test fun unsignedIdentitiesAndTerminalLossRetainTheirBitsAndPresence() {
    val registration = OrderedPortRegistration(ULong.MAX_VALUE, 1uL shl 63, ULong.MAX_VALUE)
    val loss = OrderedPortLoss("member", ULong.MAX_VALUE, "source-records", null, 0uL)
    val batch = OrderedGroupBridgeMapper.records(OrderedPortBatch(ULong.MAX_VALUE, listOf(
      OrderedPortRecord.Value(registration, ULong.MAX_VALUE, byteArrayOf(-1)),
      OrderedPortRecord.Terminal("overflow", true, OrderedPortRejected.Loss(loss), false,
        nativeDomain = "android", nativeCode = "native-error", failureMember = registration),
      OrderedPortRecord.Terminal("source-failed", true, OrderedPortRejected.Unmapped, false)
    )))
    assertTrue(batch.hasBatch)
    assertEquals(-1L, batch.batch)
    assertEquals(-1L, batch.records[0].member.member)
    assertEquals(Long.MIN_VALUE, batch.records[0].member.sourceMember)
    assertEquals(-1L, batch.records[0].ordinal)
    val terminal = batch.records[1]
    assertEquals("loss", terminal.rejectedKind)
    assertEquals(-1L, terminal.scopeMember)
    assertFalse(terminal.hasCount)
    assertTrue(terminal.hasByteCount)
    assertEquals(0L, terminal.byteCount)
    assertEquals("android", terminal.nativeDomain)
    assertEquals("native-error", terminal.nativeCode)
    assertEquals(-1L, terminal.failureMember.member)
    assertEquals("unmapped", batch.records[2].rejectedKind)
  }

  @Test fun markerAndRejectionPreserveUnsignedBitsAndOrder() {
    val batch = OrderedGroupBridgeMapper.records(OrderedPortBatch(1uL, listOf(
      OrderedPortRecord.Marker(ULong.MAX_VALUE, 1uL shl 63),
      OrderedPortRecord.Terminal("overflow", true, OrderedPortRejected.Marker(ULong.MAX_VALUE), false)
    )))
    assertEquals("marker", batch.records[0].kind)
    assertEquals(-1L, batch.records[0].markerId)
    assertEquals(Long.MIN_VALUE, batch.records[0].ordinal)
    assertEquals("marker", batch.records[1].rejectedKind)
    assertEquals(-1L, batch.records[1].rejectedMarkerId)
  }

  @Test fun sourceGenerationRequiresCanonicalCounter() {
    assertEquals(ULong.MAX_VALUE, OrderedGroupBridgeMapper.generation("counter", "18446744073709551615"))
    try { OrderedGroupBridgeMapper.generation("counter", "01"); fail("noncanonical generation accepted") }
    catch (_: IllegalArgumentException) { }
    try { OrderedGroupBridgeMapper.generation("opaque", "1"); fail("opaque generation accepted") }
    catch (_: IllegalArgumentException) { }
  }
}
