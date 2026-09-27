package com.sfourdrinier.unifiedblemanager.presence

/** Queued-backlog proof without consuming it. */
data class BacklogCounts(val queuedBytes: Long?, val ingressDrops: Map<String, Long>)

/**
 * Replay-safe prepared handoff from the shared native owner. No-wake is
 * tokenless and empty. A retained failure never authorizes forgetting native
 * resources; only the acknowledgement's confirmed disposal retires ownership.
 */
data class ContinuationClaim(
  val consumerCount: Int,
  val batches: List<String>,
  val disposed: Boolean,
  val disposeFailure: String? = null,
  val afterCutoffLoss: CutoffLoss = CutoffLoss(0, 0),
  val selectors: List<ContinuationSelector> = emptyList(),
  val claimToken: String = "",
  val recordingId: String? = null
)

/** Native cleanup result after the application acknowledged decoded bytes. */
data class ContinuationAcknowledgement(
  val disposed: Boolean,
  val afterCutoffLoss: CutoffLoss,
  val disposeFailure: String?
)

/** Native intake attempts observed after the sealed handoff cutoff. */
data class CutoffLoss(val items: Long, val bytes: Long)
