package com.sfourdrinier.unifiedblemanager.presence

import com.sfourdrinier.unifiedblemanager.rustcore.RustCoreJson
import com.sfourdrinier.unifiedblemanager.rustcore.RustCoreRejection

/** The OS wake boundary must persist execution failures, even before native dispatch. */
internal fun executePresenceContinuation(
  strategy: ContinuationStrategy,
  execute: () -> ContinuationOutcome
): ContinuationOutcome = try {
  execute()
} catch (error: RustCoreRejection) {
  ContinuationOutcome.failed(
    strategy,
    error.code,
    error.detail ?: "Continuation execution was refused (${error.code})",
    error.platform?.let { RustCoreJson.write(it) }
  )
} catch (error: Throwable) {
  ContinuationOutcome.failed(
    strategy,
    "lifecycle.invariant-violation",
    "continuation executor threw: ${error.message ?: error.javaClass.simpleName}",
    null
  )
}
