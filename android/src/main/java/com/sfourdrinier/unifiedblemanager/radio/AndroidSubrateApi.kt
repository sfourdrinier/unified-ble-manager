package com.sfourdrinier.unifiedblemanager.radio

import java.lang.reflect.InvocationTargetException

/** Public 36.1 API lookup keeps the package's compile-SDK 36.0 floor.
 * A preview's Boolean-returning method is deliberately not admitted. */
internal class AndroidSubrateApi(fullSdk: Int, gattClass: Class<*>) {
  private val method = if (fullSdk < 3600001) null else {
    try { gattClass.getMethod("requestSubrateMode", Int::class.javaPrimitiveType) }
    catch (_: NoSuchMethodException) { null }
  }?.takeIf { it.returnType == Int::class.javaPrimitiveType }
  val available: Boolean get() = method != null

  fun request(gatt: Any, mode: String): Int {
    val value = when (mode) {
      "default" -> 2
      "low-latency" -> 0
      "low-power" -> 1
      "high-throughput" -> 3
      else -> throw IllegalArgumentException("unknown subrate request mode '$mode'")
    }
    val request = method ?: throw UnsupportedOperationException("Android subrate requests require SDK 36.1")
    val result = try { request.invoke(gatt, value) }
    catch (error: InvocationTargetException) { throw error.targetException }
    check(result is Int) { "requestSubrateMode returned a non-integer status" }
    return result
  }
}
