package com.sfourdrinier.unifiedblemanager.rustcore

import java.util.UUID

/**
 * One owner-wide admission fence for Android retirement version 4.
 *
 * A lease is acquired before a host operation is submitted to a driver or an
 * executor. Fencing is synchronous: it advances the epoch and makes every
 * later admission fail before it can enter an unrelated component lock. A
 * caller that already owns a lease can finish and release it after its exact
 * completion is delivered. This lets a status observation distinguish work
 * admitted before the fence from work rejected by it.
 */
internal class AndroidOwnerQuiescenceCoordinator {
  internal data class Snapshot(
    val incarnation: UUID,
    val retirement: UUID?,
    val fenced: Boolean,
    val epoch: Long,
    val inFlight: Int,
    val domains: Set<String>
  )

  internal class Lease internal constructor(
    private val coordinator: AndroidOwnerQuiescenceCoordinator,
    private val token: Long
  ) : AutoCloseable {
    @Volatile private var released = false

    override fun close() {
      if (!released) {
        released = true
        coordinator.release(token)
      }
    }
  }

  private val lock = Any()
  private val incarnation = UUID.randomUUID()
  private var retirement: UUID? = null
  private var fenced = false
  private var epoch = 0L
  private var nextToken = 1L
  private val work = mutableMapOf<Long, String>()

  fun owner(): UUID = incarnation

  /** Returns null once a fence has started; no native work may be admitted. */
  fun admit(domain: String): Lease? = synchronized(lock) {
    if (fenced) return null
    val token = nextToken++
    work[token] = domain
    Lease(this, token)
  }

  /** Fences this exact owner. A conflicting retirement is never merged. */
  fun fence(owner: UUID, requestedRetirement: UUID): String? = synchronized(lock) {
    if (owner != incarnation) return "wrong-owner"
    val current = retirement
    if (current != null && current != requestedRetirement) return "conflicting-retirement"
    if (current == null) {
      retirement = requestedRetirement
      fenced = true
      epoch++
    }
    null
  }

  fun status(owner: UUID, requestedRetirement: UUID): Snapshot? = synchronized(lock) {
    if (owner != incarnation || retirement != requestedRetirement) return null
    Snapshot(incarnation, retirement, fenced, epoch, work.size, work.values.toSet())
  }

  private fun release(token: Long) = synchronized(lock) { work.remove(token) }
}
