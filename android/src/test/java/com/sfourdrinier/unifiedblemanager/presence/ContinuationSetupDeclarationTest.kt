package com.sfourdrinier.unifiedblemanager.presence

import com.sfourdrinier.unifiedblemanager.rustcore.RustCoreJson
import org.junit.Assert.*
import org.junit.Test

class ContinuationSetupDeclarationTest {
  @Test fun linkAndRecordingHaveStrictNativeOnlyBounds() {
    val mtu = mapOf("requested" to 517, "timeoutMs" to 20000, "onUnsupported" to "continue")
    val recording = mapOf("id" to "session_1", "maxBytes" to 1073741824, "maxRecords" to 1000000)
    fun parse(link: Any?, journal: Any?, strategy: String = "native") = BackgroundContinuationDeclaration.parse(
      RustCoreJson.write(mapOf("onAppearance" to strategy, "link" to link, "recording" to journal)))
    parse(mapOf("mtu" to mtu), recording)
    for (invalid in listOf(mtu + ("requested" to 22), mtu + ("requested" to 518),
      mtu + ("timeoutMs" to 0), mtu + ("timeoutMs" to 20001), mtu + ("onUnsupported" to "ignore"), mtu + ("unknown" to 1))) {
      assertThrows(IllegalArgumentException::class.java) { parse(mapOf("mtu" to invalid), recording) }
    }
    for (invalid in listOf(recording + ("id" to "../escape"), recording + ("id" to ""),
      recording + ("maxBytes" to 1048575), recording + ("maxBytes" to 1073741825),
      recording + ("maxRecords" to 0), recording + ("maxRecords" to 1000001), recording + ("path" to "/tmp"))) {
      assertThrows(IllegalArgumentException::class.java) { parse(mapOf("mtu" to mtu), invalid) }
    }
    assertThrows(IllegalArgumentException::class.java) { parse(mapOf("mtu" to mtu), recording, "record-only") }
    assertThrows(IllegalArgumentException::class.java) { parse(null, recording) }
    assertThrows(IllegalArgumentException::class.java) { parse(mapOf("mtu" to mtu), null) }
  }
  private val selector = mapOf("serviceUuid" to "0000180d-0000-1000-8000-00805f9b34fb",
    "characteristicUuid" to "00002a37-0000-1000-8000-00805f9b34fb")
  private val status = mapOf("offset" to 1, "accepted" to listOf(0, 255))
  private val response = mapOf("subscriptionIndex" to 0, "prefix" to listOf(240),
    "minLength" to 2, "maxLength" to 512, "status" to status)
  private val step = mapOf("selector" to selector, "value" to listOf(0, 255),
    "timeoutMs" to 1000, "response" to response)
  private fun declaration(steps: Any?, strategy: String = "native") = RustCoreJson.write(
    mapOf("onAppearance" to strategy, "resubscribe" to listOf(selector), "setup" to steps))

  @Test fun acceptsBoundedSetupAndOptionalResponse() {
    BackgroundContinuationDeclaration.parse(declaration(listOf(step)))
    BackgroundContinuationDeclaration.parse(declaration(List(16) { step + ("timeoutMs" to 1) }))
    BackgroundContinuationDeclaration.parse(declaration(List(3) { step + ("timeoutMs" to 20000) }))
    BackgroundContinuationDeclaration.parse(declaration(listOf(step - "response")))
  }

  @Test fun trailingResponseAllowsOnlyOneOptionalValidatedByte() {
    val trailing = mapOf("offset" to 2, "accepted" to listOf(0))
    val reply = response + mapOf("maxLength" to 3, "trailing" to trailing)
    BackgroundContinuationDeclaration.parse(declaration(listOf(step + ("response" to reply))))
    for (invalid in listOf(trailing + ("offset" to 1), trailing + ("offset" to 3),
      trailing + ("accepted" to listOf(0, 0)), trailing + ("accepted" to emptyList<Int>()),
      trailing + ("unknown" to 1))) {
      assertThrows(IllegalArgumentException::class.java) {
        BackgroundContinuationDeclaration.parse(declaration(listOf(step + ("response" to (reply + ("trailing" to invalid))))))
      }
    }
    assertThrows(IllegalArgumentException::class.java) {
      BackgroundContinuationDeclaration.parse(declaration(listOf(step + ("response" to (reply + ("maxLength" to 4))))))
    }
  }

  @Test fun rejectsEveryMalformedSetupBoundary() {
    val invalidSteps = listOf(
      step + ("unknown" to 1), step + ("selector" to (selector + ("unknown" to 1))),
      step + ("selector" to (selector + ("serviceOccurrence" to null))),
      step + ("value" to emptyList<Int>()), step + ("value" to List(513) { 0 }),
      step + ("value" to listOf(-1)), step + ("value" to listOf(256)),
      step + ("value" to listOf(true)), step + ("timeoutMs" to 0),
      step + ("timeoutMs" to 20001), step + ("response" to null),
      step + ("response" to (response + ("unknown" to 1))),
      step + ("response" to (response + ("subscriptionIndex" to 1))),
      step + ("response" to (response + ("prefix" to emptyList<Int>()))),
      step + ("response" to (response + ("prefix" to List(513) { 0 }))),
      step + ("response" to (response + ("minLength" to 0))),
      step + ("response" to (response + ("maxLength" to 1))),
      step + ("response" to (response + ("maxLength" to 513))),
      step + ("response" to (response + ("status" to (status + ("offset" to 0))))),
      step + ("response" to (response + ("status" to (status + ("offset" to 2))))),
      step + ("response" to (response + ("status" to (status + ("accepted" to listOf(0, 0)))))),
      step + ("response" to (response + ("status" to (status + ("accepted" to emptyList<Int>()))))),
      step + ("response" to (response + ("status" to (status + ("accepted" to listOf(256)))))),
      step + ("response" to (response + ("status" to (status + ("unknown" to 1)))))
    )
    for (invalid in invalidSteps) assertThrows(invalid.toString(), IllegalArgumentException::class.java) {
      BackgroundContinuationDeclaration.parse(declaration(listOf(invalid)))
    }
    for (invalid in listOf(null, mapOf<String, Any>(), List(17) { step }, List(4) { step + ("timeoutMs" to 20000) })) {
      assertThrows(IllegalArgumentException::class.java) { BackgroundContinuationDeclaration.parse(declaration(invalid)) }
    }
    assertThrows(IllegalArgumentException::class.java) {
      BackgroundContinuationDeclaration.parse(declaration(emptyList<Any>(), "record-only"))
    }
  }
}
