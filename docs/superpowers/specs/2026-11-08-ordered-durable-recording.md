# Durable recording of ordered notification groups

Status: historical design proposal. Ordered recording APIs and implementations now exist; see [Unified Semantics](../../UNIFIED_SEMANTICS.md) and the public types for the current contract. This proposal does not establish platform, process-recovery, or hardware qualification.

## Ownership and order

One native recording owner consumes one ordered collection's retained delivery FIFO. It uses the existing host-private `JournalRegistry` and `ContinuationJournal` as the sole durable recording authority. Desktop and mobile use the same journal contract; neither per-characteristic pollers nor a JavaScript relay may reconstruct or reorder the group. Existing recording export, prepare, and acknowledgment remain the reader boundary.

Each committed envelope identifies the recording and collection, peer and tagged source generation, and its ordered delivery identity. Values retain exact member identity and membership epoch, source ordinal as a lossless unsigned 64-bit value, raw bytes, provenance and delivery kind, and an optional receipt only when supplied. Loss controls retain their scope, unit, optional count and bytes. Terminals retain their reason, rejected-record evidence, uncertain-tail state and any additional failure evidence. Preserve other ordered controls needed to interpret the stream. Journal order is the FIFO order; do not invent source ordinals for controls or infer loss from ordinal gaps. Distinguish source ingress order from radio transmission order.

## Commit, acknowledgment and recovery

Retain each delivery until its journal transaction commits. The transaction must atomically persist the envelope and an idempotency identity scoped to the recording, a collision-resistant producer incarnation, exact collection/source generation and delivery ID. Transient source, collection and delivery counters may restart, so a newly created producer must obtain a fresh incarnation while retries retain the old one. A replay of the same identity and payload returns the committed result without a duplicate; a conflicting payload fails explicitly. Only then acknowledge that exact central delivery. A cancellation before commit leaves it available; cancellation after commit retries the idempotent commit and acknowledgment. Serialize one owner per collection, including stop and recovery, and retain its pending delivery and cleanup state across waiter cancellation. Batching is permitted only if the same ordered prefix and atomic identity rules hold and the existing journal batch limits are respected.

Journal reader prepare/acknowledge and source-delivery acknowledgment are distinct: source acknowledgment never deletes journal data, and reader acknowledgment never releases native radio ownership. Close seals the source, drains and commits its admitted prefix and ordered controls, then records final terminal evidence. Retain and retry exact physical cleanup and source fences; acknowledge the collection reservation only after its deliveries and cleanup obligations settle. A terminal is not proof that cleanup completed.

The journal survives process restart, but current retained central deliveries and live native source ownership are in memory. A crash before commit can lose observations after the last durable record, including after native-to-central acknowledgment. Recovery must expose an uncertain tail for an interrupted live recording; it must not claim exactly-once acquisition or resume the old physical source unless a separately specified recovery protocol proves it. Offline journal replay remains available independently of radio recovery.

## Bounded failure and loss

Use existing journal quotas and bounded source queues. A quota or storage refusal stops admission rather than silently dropping records; retain an exact delivery and cleanup responsibility until a durable result or explicit failure is established. Record the accepted prefix and preserve known rejected value counts and raw-byte counts exactly. Unknown counts, bytes, source scope, control loss and tails remain unknown, not zero or an invented per-member allocation. If the journal cannot durably record failure evidence, retain and surface the existing unpersisted storage failure truth; do not report a complete recording. The existing `RecordedLoss` aggregate alone cannot encode all ordered loss controls and requires an appropriate contract extension. Avoid claiming a durable loss marker when storage did not commit it.

## Acceptance evidence

- Interleave members and controls, including setup callbacks and loss, and verify durable replay preserves exact FIFO, identities, bytes, provenance and full-width ordinals.
- Cancel before commit and between commit and source acknowledgment; retry without duplicate records or premature acknowledgment. Verify conflicting idempotency payloads fail.
- Exercise bounded quota and storage failures, known and unknown loss, terminal sealing, cleanup refusal and retry, and independent existing subscribers.
- Restart around committed and uncommitted deliveries: replay committed data, preserve reader prepare/ack behavior, and expose interrupted-source uncertainty without claiming live recovery.
- Exercise the desktop fake and mobile native bridge; report deterministic tests, native builds and physical-radio qualification separately.
