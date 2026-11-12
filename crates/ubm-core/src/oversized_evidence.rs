//! Bounded, in-memory extraction of an already retained, flat evidence value.
//!
//! This is a core building block, not a native-source or transport integration.
//! The owner must authenticate callers, allocate globally unique identities,
//! retain the source occurrence until ACK, and serialize access to `Recovery`.
//! It must construct the flat source without recursive copying and include all
//! context needed to reconstruct the original value. Neither crash durability
//! nor platform-native bytes are implied.
//!
//! Representation `ubm-ordered-evidence/1` is a single prefix-encoded typed
//! value. Tags: null 00, false 01, true 02, signed 03, unsigned 04, float
//! bits 05, UTF-8 text 06, bytes 07, map 08, array 09. Integers, lengths,
//! IEEE-754 bits and container counts are eight-byte big-endian values.
//! Arrays contain the stated number of values; maps contain that many ordered
//! key/value pairs. Keys are values, and duplicate keys are preserved. There is
//! no normalization; arbitrary float bit patterns and byte strings survive.
//!
//! Reads are synchronous and linearized by the owner: cancellation before a
//! call changes nothing, and a discarded response can be retried. A completed
//! read records that its bytes were issued, not that a remote caller persisted
//! them. The caller's final ACK asserts successful receipt and persistence.
//! Preparation indexes one offset per token; the caller's token, depth, encoded
//! byte and chunk limits bound the work and retained bookkeeping. Chunk reads
//! seek by binary search and visit only tokens intersecting the requested range.

use std::sync::Arc;

pub const PROTOCOL: u8 = 1;
pub const REPRESENTATION: &str = "ubm-ordered-evidence/1";
pub const MAX_CHUNK_BYTES: u32 = 64 * 1024;

/// Opaque, owner-assigned identities. IDs must be unique within their scope;
/// an ordinal or a payload hash alone is not an occurrence identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvidenceIdentity {
    pub host: [u8; 16],
    pub owner: [u8; 16],
    pub peer: [u8; 16],
    pub collection: [u8; 16],
    pub source_generation: [u8; 16],
    pub group: [u8; 16],
    pub batch: [u8; 16],
    pub record_index: u64,
    pub delivery: [u8; 16],
}

/// Flat prefix tokens: containers precede their children, without recursion.
#[derive(Debug, Clone)]
pub enum EvidenceToken {
    Null,
    Bool(bool),
    I64(i64),
    U64(u64),
    F64Bits(u64),
    Text(String),
    Bytes(Vec<u8>),
    Map(u64),
    Array(u64),
}

/// Owned, immutable flat value. Construction takes ownership without copying.
#[derive(Debug, Clone)]
pub struct EvidenceSource {
    tokens: Vec<EvidenceToken>,
}

impl EvidenceSource {
    pub fn new(tokens: Vec<EvidenceToken>) -> Self {
        Self { tokens }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct EvidenceLimits {
    pub max_depth: usize,
    pub max_tokens: usize,
    pub max_encoded_bytes: u64,
    pub max_chunks: u64,
    pub chunk_bytes: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvidenceDescriptor {
    pub protocol: u8,
    pub representation: &'static str,
    pub identity: EvidenceIdentity,
    pub encoded_length: u64,
    pub chunk_bytes: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RecoveryError {
    InvalidLimits,
    DepthLimit,
    TokenLimit,
    EncodedLimit,
    ChunkLimit,
    /// Index, validation stack, receipt bitmap, or output buffer allocation
    /// could not be represented or reserved. An owner's retained reference is
    /// unaffected.
    ResourceExhausted,
    MalformedSource,
    IdentityMismatch,
    InvalidOffset,
    ChunkSizeMismatch,
    Incomplete,
    Acknowledged,
    /// The configured scan-owner bookkeeping capacity is insufficient.
    Capacity,
    NotPrepared,
    Retired,
    InvalidMembership,
}

#[derive(Debug, PartialEq, Eq)]
pub struct EvidenceChunk {
    pub offset: u64,
    pub next_offset: u64,
    pub bytes: Vec<u8>,
    pub done: bool,
}

/// One occurrence; keep this receipt available to retry a lost final ACK.
#[derive(Clone)]
pub struct Recovery {
    descriptor: EvidenceDescriptor,
    source: Option<Arc<EvidenceSource>>,
    token_offsets: Vec<u64>,
    issued: Vec<u8>,
    remaining: u64,
    #[cfg(test)]
    visited_tokens: usize,
}

/// Versioned scan envelope: a flat encoded array of exactly three values:
/// the original typed failure, source provenance, and occurrence context.
/// The producer owns the schemas of those values and must preserve all fields,
/// including unavailable-versus-present information and ordered metadata.
pub const SCAN_FAILURE_REPRESENTATION: &str = "ubm-scan-failure-evidence/1";

/// Versioned representation for one retained nonterminal peer-source failure.
/// The producing host owns the evidence bytes and validates every read/ACK;
/// these identities only bind a descriptor to one immutable ingress head.
pub const PEER_SOURCE_FAILURE_REPRESENTATION: &str = "ubm-peer-source-failure/1";

/// Opaque identity for one peer notification source failure occurrence.
/// Every component is assigned by the owning host. None may be reconstructed
/// from payload bytes or reused by a later reconnect/source transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PeerSourceFailureIdentity {
    pub host: [u8; 16],
    pub peer: [u8; 16],
    pub source_generation: [u8; 16],
    pub native_group: [u8; 16],
    pub transfer: [u8; 16],
    pub recovery: [u8; 16],
}

/// A capability to page and acknowledge one peer-source failure occurrence.
/// The descriptor is group-level: its owner separately retains the immutable
/// source-time recipient cut and may advance the source only after every
/// recipient accepts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PeerSourceFailureDescriptor {
    pub protocol: u8,
    pub representation: &'static str,
    pub identity: PeerSourceFailureIdentity,
    pub encoded_length: u64,
    pub chunk_bytes: u32,
}

/// One deterministic bounded page from a peer-source failure recovery.
#[derive(Debug, PartialEq, Eq)]
pub struct PeerSourceFailureChunk {
    pub offset: u64,
    pub next_offset: u64,
    pub bytes: Vec<u8>,
    pub done: bool,
}

/// Opaque identities assigned by the owning authority, never derived solely
/// from a payload or a reusable scan ID. The owner authenticates all callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanFailureIdentity {
    pub host: [u8; 16],
    pub scan: [u8; 16],
    pub start: [u8; 16],
    pub source_generation: [u8; 16],
    pub occurrence: [u8; 16],
}

/// One member of the immutable callback-time recipient cut. Delivery IDs must
/// be unique for the lifetime of the owner's retained and retired receipts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanFailureMember {
    pub recipient: [u8; 16],
    pub delivery: [u8; 16],
}

#[derive(Debug, Clone, Copy)]
pub struct ScanFailureLimits {
    pub evidence: EvidenceLimits,
    pub max_recipients: usize,
    /// Upper bound for logical source token offsets, validation stack entries,
    /// shared read bitmap, per-recipient receipt storage, and membership index. Excludes the
    /// already owned source, member vector, allocator rounding and overhead,
    /// and one bounded output chunk; the host must account for those separately.
    pub max_bookkeeping_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanFailureDescriptor {
    pub protocol: u8,
    pub representation: &'static str,
    pub identity: ScanFailureIdentity,
    pub member: ScanFailureMember,
    pub encoded_length: u64,
    pub chunk_bytes: u32,
}

/// Durable, transport-neutral state for one prepared scan recipient.  This is
/// deliberately a value snapshot: the owner must persist it atomically with
/// the transition that changed it and rehydrate it only against the exact raw
/// source-time cut.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanFailureReceiptSnapshot {
    pub issued: Vec<u8>,
    pub remaining: u64,
    pub acknowledged: bool,
    pub retired: bool,
}

/// Complete mutable state needed to restore a prepared scan occurrence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanFailurePreparedSnapshot {
    pub encoded_length: u64,
    pub chunk_bytes: u32,
    pub receipts: Vec<ScanFailureReceiptSnapshot>,
}

#[derive(Clone)]
struct ScanReceipt {
    issued: Vec<u8>,
    remaining: u64,
    acknowledged: bool,
    retired: bool,
}

/// A single immutable occurrence and its exact recipient obligations.
///
/// Construct only after reserving the owner slot and membership storage, before
/// releasing the upstream head. `prepare` is retryable after any failure and
/// never consumes the head. The caller serializes calls and keeps this object,
/// including retired tombstones, until its surrounding lease can safely end.
/// No operation here authenticates, persists, transports, or captures a failure.
#[derive(Clone)]
pub struct ScanFailureOccurrence {
    identity: ScanFailureIdentity,
    members: Vec<ScanFailureMember>,
    source: Option<Arc<EvidenceSource>>,
    reader: Option<Recovery>,
    descriptor: Option<EvidenceDescriptor>,
    receipts: Vec<ScanReceipt>,
    member_index: Vec<usize>,
    unacknowledged: usize,
    unretired: usize,
    prepared_bookkeeping_bytes: usize,
}

impl ScanFailureOccurrence {
    pub fn new(
        identity: ScanFailureIdentity,
        members: Vec<ScanFailureMember>,
        source: Arc<EvidenceSource>,
    ) -> Self {
        Self {
            identity,
            members,
            source: Some(source),
            reader: None,
            descriptor: None,
            receipts: Vec::new(),
            member_index: Vec::new(),
            unacknowledged: 0,
            unretired: 0,
            prepared_bookkeeping_bytes: 0,
        }
    }

    pub fn identity(&self) -> ScanFailureIdentity {
        self.identity
    }
    pub fn members(&self) -> &[ScanFailureMember] {
        &self.members
    }

    /// Borrow the exact flat source while it is retained, including malformed
    /// tokens. No parsing, validation, or source copy is performed.
    pub fn source_tokens(&self) -> Option<&[EvidenceToken]> {
        self.source.as_ref().map(|source| source.tokens.as_slice())
    }

    /// Capacity of the owned source and membership allocations. Shared source
    /// Arcs are charged in full to every retaining owner. Allocator metadata,
    /// rounding, and the Arc control block are not included. After the last
    /// ACK releases the source, this still charges the retained member vector.
    pub fn retained_source_capacity(&self, limit: u64) -> Option<u64> {
        let mut bytes = self
            .members
            .capacity()
            .checked_mul(std::mem::size_of::<ScanFailureMember>())?;
        let Some(source) = self.source.as_ref() else {
            return u64::try_from(bytes).ok().filter(|bytes| *bytes <= limit);
        };
        bytes = bytes
            .checked_add(std::mem::size_of::<EvidenceSource>())?
            .checked_add(
                source
                    .tokens
                    .capacity()
                    .checked_mul(std::mem::size_of::<EvidenceToken>())?,
            )?;
        if u64::try_from(bytes).ok()? > limit {
            return None;
        }
        for token in &source.tokens {
            let capacity = match token {
                EvidenceToken::Text(value) => value.capacity(),
                EvidenceToken::Bytes(value) => value.capacity(),
                _ => 0,
            };
            bytes = bytes.checked_add(capacity)?;
            if u64::try_from(bytes).ok()? > limit {
                return None;
            }
        }
        u64::try_from(bytes).ok()
    }

    /// Retained bookkeeping allocation capacities for the prepared occurrence.
    /// This shrinks as ACKs release bitmaps and the shared reader, while the
    /// membership index and receipt tombstones remain charged.
    pub fn prepared_bookkeeping_bytes(&self) -> Option<usize> {
        self.descriptor.map(|_| self.prepared_bookkeeping_bytes)
    }

    /// Capture the mutable receipt state after preparation.  Raw source and
    /// membership are intentionally not duplicated here; durable callers keep
    /// those in their source-cut store.
    pub fn prepared_snapshot(&self) -> Option<ScanFailurePreparedSnapshot> {
        let descriptor = self.descriptor?;
        Some(ScanFailurePreparedSnapshot {
            encoded_length: descriptor.encoded_length,
            chunk_bytes: descriptor.chunk_bytes,
            receipts: self
                .receipts
                .iter()
                .map(|receipt| ScanFailureReceiptSnapshot {
                    issued: receipt.issued.clone(),
                    remaining: receipt.remaining,
                    acknowledged: receipt.acknowledged,
                    retired: receipt.retired,
                })
                .collect(),
        })
    }

    /// Restore a prepared occurrence from an exact raw source cut.  The
    /// normal preparation path is run first, then every persisted fact is
    /// checked against its derived shape before it becomes observable.
    pub fn restore_prepared_snapshot(
        &mut self,
        limits: ScanFailureLimits,
        snapshot: &ScanFailurePreparedSnapshot,
    ) -> Result<(), RecoveryError> {
        self.prepare(limits)?;
        let descriptor = self.descriptor.ok_or(RecoveryError::NotPrepared)?;
        if descriptor.encoded_length != snapshot.encoded_length
            || descriptor.chunk_bytes != snapshot.chunk_bytes
            || snapshot.receipts.len() != self.receipts.len()
        {
            return Err(RecoveryError::IdentityMismatch);
        }
        let chunks = descriptor
            .encoded_length
            .div_ceil(u64::from(descriptor.chunk_bytes));
        let bitmap = usize::try_from(chunks.div_ceil(8)).map_err(|_| RecoveryError::Capacity)?;
        let mut unacknowledged = 0usize;
        let mut unretired = 0usize;
        for (receipt, persisted) in self.receipts.iter_mut().zip(&snapshot.receipts) {
            if persisted.retired && !persisted.acknowledged
                || (!persisted.acknowledged && persisted.issued.len() != bitmap)
                || (persisted.acknowledged && !persisted.issued.is_empty())
                || persisted.remaining > chunks
                || (!persisted.acknowledged
                    && persisted.remaining
                        != chunks.saturating_sub(
                            persisted
                                .issued
                                .iter()
                                .map(|byte| byte.count_ones() as u64)
                                .sum::<u64>(),
                        ))
            {
                return Err(RecoveryError::IdentityMismatch);
            }
            receipt.issued = persisted.issued.clone();
            receipt.remaining = persisted.remaining;
            receipt.acknowledged = persisted.acknowledged;
            receipt.retired = persisted.retired;
            if !receipt.acknowledged {
                unacknowledged += 1;
            }
            if !receipt.retired {
                unretired += 1;
            }
        }
        self.unacknowledged = unacknowledged;
        self.unretired = unretired;
        if unacknowledged == 0 {
            self.reader = None;
            self.source = None;
        }
        Ok(())
    }

    /// Admission performs no whole-value serialization or source cloning.
    /// Successful preparation freezes the descriptor even if retried with
    /// different limits; failures leave the occurrence and membership intact.
    pub fn prepare(&mut self, limits: ScanFailureLimits) -> Result<(), RecoveryError> {
        if self.descriptor.is_some() {
            return Ok(());
        }
        if self.members.is_empty() || self.members.len() > limits.max_recipients {
            return Err(RecoveryError::Capacity);
        }
        let count = self.members.len();
        let member_budget = count
            .checked_mul(std::mem::size_of::<ScanReceipt>() + std::mem::size_of::<usize>())
            .ok_or(RecoveryError::Capacity)?;
        if member_budget > limits.max_bookkeeping_bytes {
            return Err(RecoveryError::Capacity);
        }
        let source = self.source.clone().ok_or(RecoveryError::Acknowledged)?;
        if !matches!(source.tokens.first(), Some(EvidenceToken::Array(3))) {
            return Err(RecoveryError::MalformedSource);
        }
        let bounds = limits.evidence;
        if bounds.max_depth == 0
            || bounds.max_tokens == 0
            || bounds.max_chunks == 0
            || bounds.chunk_bytes == 0
            || bounds.chunk_bytes > MAX_CHUNK_BYTES
        {
            return Err(RecoveryError::InvalidLimits);
        }
        if source.tokens.len() > bounds.max_tokens {
            return Err(RecoveryError::TokenLimit);
        }
        let mut length = 0u64;
        for token in &source.tokens {
            let size = match token {
                EvidenceToken::Null | EvidenceToken::Bool(_) => 1,
                EvidenceToken::Text(value) => 9u64
                    .checked_add(value.len() as u64)
                    .ok_or(RecoveryError::EncodedLimit)?,
                EvidenceToken::Bytes(value) => 9u64
                    .checked_add(value.len() as u64)
                    .ok_or(RecoveryError::EncodedLimit)?,
                _ => 9,
            };
            length = length
                .checked_add(size)
                .filter(|n| *n <= bounds.max_encoded_bytes)
                .ok_or(RecoveryError::EncodedLimit)?;
        }
        let chunks = length.div_ceil(u64::from(bounds.chunk_bytes));
        if chunks > bounds.max_chunks {
            return Err(RecoveryError::ChunkLimit);
        }
        let bitmap = usize::try_from(chunks.div_ceil(8)).map_err(|_| RecoveryError::Capacity)?;
        let budget = source
            .tokens
            .len()
            .checked_mul(std::mem::size_of::<u64>())
            .and_then(|n| {
                n.checked_add(
                    source
                        .tokens
                        .len()
                        .min(bounds.max_depth)
                        .checked_add(1)?
                        .checked_mul(std::mem::size_of::<u64>())?,
                )
            })
            .and_then(|n| n.checked_add(bitmap.checked_mul(count.checked_add(1)?)?))
            .and_then(|n| n.checked_add(member_budget))
            .ok_or(RecoveryError::Capacity)?;
        if budget > limits.max_bookkeeping_bytes {
            return Err(RecoveryError::Capacity);
        }

        // Sort one bounded index twice to validate both independent identity
        // namespaces, then retain it ordered by recipient for exact lookup.
        let mut member_index = try_vec_with_exact_capacity(count)?;
        member_index.extend(0..count);
        member_index.sort_unstable_by_key(|&i| self.members[i].delivery);
        if member_index
            .windows(2)
            .any(|pair| self.members[pair[0]].delivery == self.members[pair[1]].delivery)
        {
            return Err(RecoveryError::InvalidMembership);
        }
        member_index.sort_unstable_by_key(|&i| self.members[i].recipient);
        if member_index
            .windows(2)
            .any(|pair| self.members[pair[0]].recipient == self.members[pair[1]].recipient)
        {
            return Err(RecoveryError::InvalidMembership);
        }

        let internal_identity = EvidenceIdentity {
            host: self.identity.host,
            owner: self.identity.scan,
            peer: self.identity.start,
            collection: self.identity.occurrence,
            source_generation: self.identity.source_generation,
            group: [0; 16],
            batch: [0; 16],
            record_index: 0,
            delivery: [0; 16],
        };
        let index_bytes = member_index
            .capacity()
            .checked_mul(std::mem::size_of::<usize>())
            .ok_or(RecoveryError::Capacity)?;
        let reader_budget = limits
            .max_bookkeeping_bytes
            .checked_sub(index_bytes)
            .ok_or(RecoveryError::Capacity)?;
        let reader = Recovery::prepare_inner(
            internal_identity,
            Arc::clone(&source),
            bounds,
            Some(reader_budget),
        )?;
        let descriptor = reader.descriptor();
        let mut receipts = try_vec_with_exact_capacity(count)?;
        for _ in 0..count {
            let mut issued = try_vec_with_exact_capacity(bitmap)?;
            issued.resize(bitmap, 0);
            receipts.push(ScanReceipt {
                issued,
                remaining: chunks,
                acknowledged: false,
                retired: false,
            });
        }
        let retained = receipts
            .capacity()
            .checked_mul(std::mem::size_of::<ScanReceipt>())
            .and_then(|n| {
                n.checked_add(
                    member_index
                        .capacity()
                        .checked_mul(std::mem::size_of::<usize>())?,
                )
            })
            .and_then(|n| {
                n.checked_add(
                    reader
                        .token_offsets
                        .capacity()
                        .checked_mul(std::mem::size_of::<u64>())?,
                )
            })
            .and_then(|n| n.checked_add(reader.issued.capacity()))
            .and_then(|n| {
                receipts
                    .iter()
                    .try_fold(n, |sum, receipt| sum.checked_add(receipt.issued.capacity()))
            })
            .filter(|n| *n <= limits.max_bookkeeping_bytes)
            .ok_or(RecoveryError::Capacity)?;
        self.reader = Some(reader);
        self.descriptor = Some(descriptor);
        self.receipts = receipts;
        self.member_index = member_index;
        self.unacknowledged = count;
        self.unretired = count;
        self.prepared_bookkeeping_bytes = retained;
        Ok(())
    }

    pub fn descriptor(
        &self,
        member: ScanFailureMember,
    ) -> Result<ScanFailureDescriptor, RecoveryError> {
        let base = self.descriptor.ok_or(RecoveryError::NotPrepared)?;
        let index = self.member_index(member)?;
        if self.receipts[index].retired {
            return Err(RecoveryError::Retired);
        }
        Ok(ScanFailureDescriptor {
            protocol: PROTOCOL,
            representation: SCAN_FAILURE_REPRESENTATION,
            identity: self.identity,
            member,
            encoded_length: base.encoded_length,
            chunk_bytes: base.chunk_bytes,
        })
    }

    fn exact_index(&self, descriptor: &ScanFailureDescriptor) -> Result<usize, RecoveryError> {
        let base = self.descriptor.ok_or(RecoveryError::NotPrepared)?;
        if descriptor.protocol != PROTOCOL
            || descriptor.representation != SCAN_FAILURE_REPRESENTATION
            || descriptor.identity != self.identity
            || descriptor.encoded_length != base.encoded_length
            || descriptor.chunk_bytes != base.chunk_bytes
        {
            return Err(RecoveryError::IdentityMismatch);
        }
        self.member_index(descriptor.member)
    }

    fn member_index(&self, member: ScanFailureMember) -> Result<usize, RecoveryError> {
        let position = self
            .member_index
            .binary_search_by_key(&member.recipient, |&i| self.members[i].recipient)
            .map_err(|_| RecoveryError::IdentityMismatch)?;
        let index = self.member_index[position];
        if self.members[index] != member {
            return Err(RecoveryError::IdentityMismatch);
        }
        Ok(index)
    }

    /// A canceled call before entry changes nothing. A lost completed reply may
    /// be replayed until ACK; issuance is not proof of remote persistence.
    pub fn read(
        &mut self,
        descriptor: &ScanFailureDescriptor,
        offset: u64,
    ) -> Result<EvidenceChunk, RecoveryError> {
        let index = self.exact_index(descriptor)?;
        let receipt = &mut self.receipts[index];
        if receipt.retired {
            return Err(RecoveryError::Retired);
        }
        if receipt.acknowledged {
            return Err(RecoveryError::Acknowledged);
        }
        let base = self.descriptor.ok_or(RecoveryError::NotPrepared)?;
        let chunk = self
            .reader
            .as_mut()
            .ok_or(RecoveryError::Acknowledged)?
            .read(&base, offset, descriptor.chunk_bytes)?;
        let chunk_index = offset / u64::from(descriptor.chunk_bytes);
        let byte = &mut receipt.issued[(chunk_index / 8) as usize];
        let mask = 1u8 << (chunk_index % 8);
        if *byte & mask == 0 {
            *byte |= mask;
            receipt.remaining -= 1;
        }
        Ok(chunk)
    }

    /// Exact receipt progress for a descriptor owned by this occurrence. This
    /// is a projection of the persisted receipt bitmap, not an inference from
    /// requests. It is intended for trusted transport status only.
    pub fn receipt_progress(
        &self,
        descriptor: &ScanFailureDescriptor,
    ) -> Result<(u64, bool), RecoveryError> {
        let index = self.exact_index(descriptor)?;
        let receipt = &self.receipts[index];
        if receipt.acknowledged {
            return Ok((descriptor.encoded_length, true));
        }
        let chunks = descriptor
            .encoded_length
            .div_ceil(u64::from(descriptor.chunk_bytes));
        let mut contiguous_chunks = 0_u64;
        while contiguous_chunks < chunks {
            let byte = receipt.issued[(contiguous_chunks / 8) as usize];
            if byte & (1 << (contiguous_chunks % 8)) == 0 {
                break;
            }
            contiguous_chunks += 1;
        }
        Ok((
            contiguous_chunks
                .saturating_mul(u64::from(descriptor.chunk_bytes))
                .min(descriptor.encoded_length),
            false,
        ))
    }

    /// An exact ACK retry returns its original receipt through retirement,
    /// while the owner retains the tombstone and authenticates the caller.
    pub fn acknowledge(&mut self, descriptor: &ScanFailureDescriptor) -> Result<(), RecoveryError> {
        let index = self.exact_index(descriptor)?;
        let receipt = &mut self.receipts[index];
        if receipt.acknowledged {
            return Ok(());
        }
        if receipt.remaining != 0 {
            return Err(RecoveryError::Incomplete);
        }
        receipt.acknowledged = true;
        self.prepared_bookkeeping_bytes -= receipt.issued.capacity();
        receipt.issued = Vec::new();
        self.unacknowledged -= 1;
        if self.unacknowledged == 0 {
            if let Some(reader) = &self.reader {
                self.prepared_bookkeeping_bytes -= reader.token_offsets.capacity()
                    * std::mem::size_of::<u64>()
                    + reader.issued.capacity();
            }
            self.reader = None;
            self.source = None;
        }
        Ok(())
    }

    /// Client confirmation that an ACK reply was observed. Retrying retirement
    /// is idempotent while the enclosing owner retains this tombstone.
    pub fn retire(&mut self, descriptor: &ScanFailureDescriptor) -> Result<(), RecoveryError> {
        let index = self.exact_index(descriptor)?;
        let receipt = &mut self.receipts[index];
        if !receipt.acknowledged {
            return Err(RecoveryError::Incomplete);
        }
        if !receipt.retired {
            receipt.retired = true;
            self.unretired -= 1;
        }
        Ok(())
    }

    pub fn is_retired(&self) -> bool {
        self.descriptor.is_some() && self.unretired == 0
    }
}

// Vec::try_reserve_exact may provide a larger capacity than requested. Scan
// preparation needs its allocations to match the preflight budget *before*
// they coexist, including on allocators that round allocation sizes.
fn try_vec_with_exact_capacity<T>(count: usize) -> Result<Vec<T>, RecoveryError> {
    if count == 0 {
        return Ok(Vec::new());
    }
    let layout =
        std::alloc::Layout::array::<T>(count).map_err(|_| RecoveryError::ResourceExhausted)?;
    if layout.size() == 0 {
        return Err(RecoveryError::ResourceExhausted);
    }
    let ptr = unsafe { std::alloc::alloc(layout) }.cast::<T>();
    if ptr.is_null() {
        return Err(RecoveryError::ResourceExhausted);
    }
    // SAFETY: the global allocator returned exactly this layout for `count`
    // elements; no elements have been initialized. Vec uses the same allocator.
    Ok(unsafe { Vec::from_raw_parts(ptr, 0, count) })
}

impl Recovery {
    pub fn prepare(
        identity: EvidenceIdentity,
        source: Arc<EvidenceSource>,
        limits: EvidenceLimits,
    ) -> Result<Self, RecoveryError> {
        Self::prepare_inner(identity, source, limits, None)
    }

    fn prepare_inner(
        identity: EvidenceIdentity,
        source: Arc<EvidenceSource>,
        limits: EvidenceLimits,
        bookkeeping_budget: Option<usize>,
    ) -> Result<Self, RecoveryError> {
        if limits.max_depth == 0
            || limits.max_tokens == 0
            || limits.max_chunks == 0
            || limits.chunk_bytes == 0
            || limits.chunk_bytes > MAX_CHUNK_BYTES
        {
            return Err(RecoveryError::InvalidLimits);
        }
        if source.tokens.len() > limits.max_tokens {
            return Err(RecoveryError::TokenLimit);
        }
        let mut token_offsets = if bookkeeping_budget.is_some() {
            try_vec_with_exact_capacity(source.tokens.len())?
        } else {
            let mut offsets = Vec::new();
            offsets
                .try_reserve_exact(source.tokens.len())
                .map_err(|_| RecoveryError::ResourceExhausted)?;
            offsets
        };
        let mut slots = if let Some(budget) = bookkeeping_budget {
            let count = source
                .tokens
                .len()
                .min(limits.max_depth)
                .checked_add(1)
                .ok_or(RecoveryError::Capacity)?;
            let stack_bytes = count
                .checked_mul(std::mem::size_of::<u64>())
                .ok_or(RecoveryError::Capacity)?;
            let offsets_bytes = token_offsets
                .capacity()
                .checked_mul(std::mem::size_of::<u64>())
                .ok_or(RecoveryError::Capacity)?;
            if offsets_bytes
                .checked_add(stack_bytes)
                .filter(|bytes| *bytes <= budget)
                .is_none()
            {
                return Err(RecoveryError::Capacity);
            }
            try_vec_with_exact_capacity(count)?
        } else {
            let mut slots = Vec::new();
            slots
                .try_reserve(1)
                .map_err(|_| RecoveryError::ResourceExhausted)?;
            slots
        };
        slots.push(1u64);
        let mut length = 0u64;
        for token in &source.tokens {
            while slots.last() == Some(&0) {
                slots.pop();
            }
            let remaining = slots.last_mut().ok_or(RecoveryError::MalformedSource)?;
            *remaining -= 1;
            let (size, children) = match token {
                EvidenceToken::Null | EvidenceToken::Bool(_) => (1, None),
                EvidenceToken::I64(_) | EvidenceToken::U64(_) | EvidenceToken::F64Bits(_) => {
                    (9, None)
                }
                EvidenceToken::Text(text) => (
                    9u64.checked_add(text.len() as u64)
                        .ok_or(RecoveryError::EncodedLimit)?,
                    None,
                ),
                EvidenceToken::Bytes(bytes) => (
                    9u64.checked_add(bytes.len() as u64)
                        .ok_or(RecoveryError::EncodedLimit)?,
                    None,
                ),
                EvidenceToken::Array(count) => (9, Some(*count)),
                EvidenceToken::Map(count) => (
                    9,
                    Some(count.checked_mul(2).ok_or(RecoveryError::MalformedSource)?),
                ),
            };
            token_offsets.push(length);
            length = length
                .checked_add(size)
                .filter(|n| *n <= limits.max_encoded_bytes)
                .ok_or(RecoveryError::EncodedLimit)?;
            if let Some(count) = children {
                if slots.len() > limits.max_depth {
                    return Err(RecoveryError::DepthLimit);
                }
                if bookkeeping_budget.is_some() && slots.len() == slots.capacity() {
                    return Err(RecoveryError::Capacity);
                }
                slots
                    .try_reserve(1)
                    .map_err(|_| RecoveryError::ResourceExhausted)?;
                slots.push(count);
            }
        }
        if slots.iter().any(|remaining| *remaining != 0) {
            return Err(RecoveryError::MalformedSource);
        }
        drop(slots);
        let chunks = length.div_ceil(u64::from(limits.chunk_bytes));
        if chunks > limits.max_chunks {
            return Err(RecoveryError::ChunkLimit);
        }
        let bitmap_len =
            usize::try_from(chunks.div_ceil(8)).map_err(|_| RecoveryError::ResourceExhausted)?;
        if let Some(budget) = bookkeeping_budget {
            token_offsets
                .capacity()
                .checked_mul(std::mem::size_of::<u64>())
                .and_then(|bytes| bytes.checked_add(bitmap_len))
                .filter(|bytes| *bytes <= budget)
                .ok_or(RecoveryError::Capacity)?;
        }
        let mut issued = if bookkeeping_budget.is_some() {
            try_vec_with_exact_capacity(bitmap_len)?
        } else {
            let mut issued = Vec::new();
            issued
                .try_reserve_exact(bitmap_len)
                .map_err(|_| RecoveryError::ResourceExhausted)?;
            issued
        };
        issued.resize(bitmap_len, 0);
        Ok(Self {
            descriptor: EvidenceDescriptor {
                protocol: PROTOCOL,
                representation: REPRESENTATION,
                identity,
                encoded_length: length,
                chunk_bytes: limits.chunk_bytes,
            },
            source: Some(source),
            token_offsets,
            issued,
            remaining: chunks,
            #[cfg(test)]
            visited_tokens: 0,
        })
    }

    pub fn descriptor(&self) -> EvidenceDescriptor {
        self.descriptor
    }

    #[cfg(test)]
    fn record_token_visit(&mut self) {
        self.visited_tokens += 1;
    }

    #[cfg(not(test))]
    fn record_token_visit(&mut self) {}

    pub fn read(
        &mut self,
        descriptor: &EvidenceDescriptor,
        offset: u64,
        chunk_bytes: u32,
    ) -> Result<EvidenceChunk, RecoveryError> {
        if *descriptor != self.descriptor {
            return Err(RecoveryError::IdentityMismatch);
        }
        let source = self.source.clone().ok_or(RecoveryError::Acknowledged)?;
        if chunk_bytes != descriptor.chunk_bytes {
            return Err(RecoveryError::ChunkSizeMismatch);
        }
        if offset >= descriptor.encoded_length || !offset.is_multiple_of(u64::from(chunk_bytes)) {
            return Err(RecoveryError::InvalidOffset);
        }
        let end = offset
            .saturating_add(u64::from(chunk_bytes))
            .min(descriptor.encoded_length);
        let token_index = self.token_offsets.partition_point(|start| *start <= offset) - 1;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact((end - offset) as usize)
            .map_err(|_| RecoveryError::ResourceExhausted)?;
        let mut writer = Window {
            offset,
            end,
            position: self.token_offsets[token_index],
            bytes,
        };
        for token in &source.tokens[token_index..] {
            if writer.position >= end {
                break;
            }
            self.record_token_visit();
            match token {
                EvidenceToken::Null => writer.write(&[0]),
                EvidenceToken::Bool(value) => writer.write(&[if *value { 2 } else { 1 }]),
                EvidenceToken::I64(value) => {
                    writer.write(&[3]);
                    writer.write(&value.to_be_bytes());
                }
                EvidenceToken::U64(value) => {
                    writer.write(&[4]);
                    writer.write(&value.to_be_bytes());
                }
                EvidenceToken::F64Bits(value) => {
                    writer.write(&[5]);
                    writer.write(&value.to_be_bytes());
                }
                EvidenceToken::Text(value) => {
                    writer.write(&[6]);
                    writer.write(&(value.len() as u64).to_be_bytes());
                    writer.write(value.as_bytes());
                }
                EvidenceToken::Bytes(value) => {
                    writer.write(&[7]);
                    writer.write(&(value.len() as u64).to_be_bytes());
                    writer.write(value);
                }
                EvidenceToken::Map(count) => {
                    writer.write(&[8]);
                    writer.write(&count.to_be_bytes());
                }
                EvidenceToken::Array(count) => {
                    writer.write(&[9]);
                    writer.write(&count.to_be_bytes());
                }
            }
        }
        let index = offset / u64::from(chunk_bytes);
        let byte = &mut self.issued[(index / 8) as usize];
        let mask = 1u8 << (index % 8);
        if *byte & mask == 0 {
            *byte |= mask;
            self.remaining -= 1;
        }
        Ok(EvidenceChunk {
            offset,
            next_offset: end,
            bytes: writer.bytes,
            done: end == descriptor.encoded_length,
        })
    }

    pub fn acknowledge(&mut self, descriptor: &EvidenceDescriptor) -> Result<(), RecoveryError> {
        if *descriptor != self.descriptor {
            return Err(RecoveryError::IdentityMismatch);
        }
        if self.remaining != 0 {
            return Err(RecoveryError::Incomplete);
        }
        self.source = None;
        self.token_offsets = Vec::new();
        self.issued = Vec::new();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    struct MeasuredAllocator;
    thread_local! {
        static ALLOCATION_MEASUREMENT: Cell<Option<(usize, usize)>> = const { Cell::new(None) };
        static FAIL_ALLOCATION: Cell<usize> = const { Cell::new(0) };
    }

    #[test]
    fn peer_source_failure_descriptor_keeps_exact_ingress_head_identity() {
        let descriptor = PeerSourceFailureDescriptor {
            protocol: PROTOCOL,
            representation: PEER_SOURCE_FAILURE_REPRESENTATION,
            identity: PeerSourceFailureIdentity {
                host: [1; 16],
                peer: [2; 16],
                source_generation: [3; 16],
                native_group: [4; 16],
                transfer: [5; 16],
                recovery: [6; 16],
            },
            encoded_length: 73,
            chunk_bytes: MAX_CHUNK_BYTES,
        };
        assert_eq!(descriptor.protocol, 1);
        assert_eq!(descriptor.representation, "ubm-peer-source-failure/1");
        assert_ne!(descriptor.identity.transfer, descriptor.identity.recovery);
    }

    fn fail_this_allocation() -> bool {
        FAIL_ALLOCATION
            .try_with(|count| {
                let remaining = count.get();
                if remaining == 0 {
                    return false;
                }
                count.set(remaining - 1);
                remaining == 1
            })
            .unwrap_or(false)
    }

    #[global_allocator]
    static ALLOCATOR: MeasuredAllocator = MeasuredAllocator;

    unsafe impl GlobalAlloc for MeasuredAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            if fail_this_allocation() {
                return std::ptr::null_mut();
            }
            let ptr = unsafe { System.alloc(layout) };
            if !ptr.is_null() {
                let _ = ALLOCATION_MEASUREMENT.try_with(|state| {
                    if let Some((live, peak)) = state.get() {
                        let live = live + layout.size();
                        state.set(Some((live, peak.max(live))));
                    }
                });
            }
            ptr
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            let _ = ALLOCATION_MEASUREMENT.try_with(|state| {
                if let Some((live, peak)) = state.get() {
                    state.set(Some((live - layout.size(), peak)));
                }
            });
            unsafe { System.dealloc(ptr, layout) };
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            if fail_this_allocation() {
                return std::ptr::null_mut();
            }
            let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
            if !new_ptr.is_null() {
                let _ = ALLOCATION_MEASUREMENT.try_with(|state| {
                    if let Some((live, peak)) = state.get() {
                        let live = live - layout.size() + new_size;
                        state.set(Some((live, peak.max(live))));
                    }
                });
            }
            new_ptr
        }
    }

    #[test]
    fn scan_nested_preparation_never_exceeds_bookkeeping_budget() {
        let nested = 20;
        let mut tokens = vec![EvidenceToken::Array(3)];
        tokens.extend((0..nested).map(|_| EvidenceToken::Array(1)));
        tokens.extend([
            EvidenceToken::F64Bits(0x7ff8_0000_0000_0042),
            EvidenceToken::Text("original".into()),
            EvidenceToken::Bytes(vec![0, 255]),
        ]);
        let token_count = tokens.len();
        let source = Arc::new(EvidenceSource::new(tokens));
        let member = ScanFailureMember {
            recipient: [1; 16],
            delivery: [2; 16],
        };
        let identity = ScanFailureIdentity {
            host: [1; 16],
            scan: [2; 16],
            start: [3; 16],
            source_generation: [4; 16],
            occurrence: [5; 16],
        };
        let mut occurrence =
            ScanFailureOccurrence::new(identity, vec![member], Arc::clone(&source));
        let encoded_length = 9 * (nested + 1) + 9 + 9 + 8 + 9 + 2;
        let bitmap = (encoded_length as u64).div_ceil(64).div_ceil(8) as usize;
        let budget = token_count * std::mem::size_of::<u64>()
            + (nested + 2) * std::mem::size_of::<u64>()
            + 2 * bitmap
            + std::mem::size_of::<ScanReceipt>()
            + std::mem::size_of::<usize>();
        let limits = ScanFailureLimits {
            evidence: EvidenceLimits {
                max_depth: nested + 1,
                max_tokens: token_count,
                max_encoded_bytes: encoded_length as u64,
                max_chunks: 100,
                chunk_bytes: 64,
            },
            max_recipients: 1,
            max_bookkeeping_bytes: budget,
        };
        let mut insufficient = limits;
        insufficient.max_bookkeeping_bytes -= 1;
        assert_eq!(
            occurrence.prepare(insufficient),
            Err(RecoveryError::Capacity)
        );
        assert!(matches!(
            &occurrence.source_tokens().unwrap()[nested + 1],
            EvidenceToken::F64Bits(0x7ff8_0000_0000_0042)
        ));
        ALLOCATION_MEASUREMENT.with(|state| state.set(Some((0, 0))));
        let result = occurrence.prepare(limits);
        let peak = ALLOCATION_MEASUREMENT.with(|state| state.replace(None).unwrap().1);
        assert_eq!(result, Ok(()));
        assert!(peak <= budget, "peak {peak} exceeds budget {budget}");
        assert!(matches!(
            &occurrence.source_tokens().unwrap()[nested + 1],
            EvidenceToken::F64Bits(0x7ff8_0000_0000_0042)
        ));
        assert!(
            matches!(&occurrence.source_tokens().unwrap()[nested + 2], EvidenceToken::Text(text) if text == "original")
        );
        assert!(
            matches!(&occurrence.source_tokens().unwrap()[nested + 3], EvidenceToken::Bytes(bytes) if bytes == &[0, 255])
        );
        let descriptor = occurrence.descriptor(member).unwrap();
        let mut actual = Vec::new();
        let mut offset = 0;
        while offset < descriptor.encoded_length {
            let chunk = occurrence.read(&descriptor, offset).unwrap();
            actual.extend_from_slice(&chunk.bytes);
            offset = chunk.next_offset;
        }
        let mut expected = vec![9];
        expected.extend_from_slice(&3u64.to_be_bytes());
        for _ in 0..nested {
            expected.push(9);
            expected.extend_from_slice(&1u64.to_be_bytes());
        }
        expected.push(5);
        expected.extend_from_slice(&0x7ff8_0000_0000_0042u64.to_be_bytes());
        expected.push(6);
        expected.extend_from_slice(&8u64.to_be_bytes());
        expected.extend_from_slice(b"original");
        expected.push(7);
        expected.extend_from_slice(&2u64.to_be_bytes());
        expected.extend_from_slice(&[0, 255]);
        assert_eq!(actual, expected);
    }

    #[test]
    fn scan_stack_allocation_failure_preserves_source_for_retry() {
        let source = Arc::new(EvidenceSource::new(vec![
            EvidenceToken::Array(3),
            EvidenceToken::Array(1),
            EvidenceToken::I64(-9),
            EvidenceToken::Bytes(vec![0, 255]),
            EvidenceToken::Null,
        ]));
        let member = ScanFailureMember {
            recipient: [1; 16],
            delivery: [2; 16],
        };
        let identity = ScanFailureIdentity {
            host: [1; 16],
            scan: [2; 16],
            start: [3; 16],
            source_generation: [4; 16],
            occurrence: [5; 16],
        };
        let mut occurrence =
            ScanFailureOccurrence::new(identity, vec![member], Arc::clone(&source));
        let limits = ScanFailureLimits {
            evidence: EvidenceLimits {
                max_depth: 2,
                max_tokens: 5,
                max_encoded_bytes: 128,
                max_chunks: 128,
                chunk_bytes: 8,
            },
            max_recipients: 1,
            max_bookkeeping_bytes: 1024,
        };
        // Membership index, token offsets, then the exactly sized validation stack.
        FAIL_ALLOCATION.with(|count| count.set(3));
        let result = occurrence.prepare(limits);
        FAIL_ALLOCATION.with(|count| count.set(0));
        assert_eq!(result, Err(RecoveryError::ResourceExhausted));
        assert!(matches!(
            &occurrence.source_tokens().unwrap()[2],
            EvidenceToken::I64(-9)
        ));
        assert!(
            matches!(&occurrence.source_tokens().unwrap()[3], EvidenceToken::Bytes(bytes) if bytes == &[0, 255])
        );
        assert_eq!(occurrence.prepared_bookkeeping_bytes(), None);
        assert_eq!(occurrence.prepare(limits), Ok(()));
    }

    #[test]
    fn late_small_chunks_and_retries_visit_only_nearby_tokens() {
        let count = 100_000;
        let mut tokens = Vec::with_capacity(count + 1);
        tokens.push(EvidenceToken::Array(count as u64));
        tokens.extend((0..count).map(|_| EvidenceToken::Null));
        let identity = EvidenceIdentity {
            host: [1; 16],
            owner: [2; 16],
            peer: [3; 16],
            collection: [4; 16],
            source_generation: [5; 16],
            group: [6; 16],
            batch: [7; 16],
            record_index: 8,
            delivery: [9; 16],
        };
        let limits = EvidenceLimits {
            max_depth: 1,
            max_tokens: count + 1,
            max_encoded_bytes: 100_009,
            max_chunks: 100_009,
            chunk_bytes: 1,
        };
        let mut recovery =
            Recovery::prepare(identity, Arc::new(EvidenceSource::new(tokens)), limits).unwrap();
        let descriptor = recovery.descriptor();
        for offset in (descriptor.encoded_length - 64..descriptor.encoded_length).rev() {
            let first = recovery.read(&descriptor, offset, 1).unwrap();
            assert_eq!(first.bytes, [0]);
            assert_eq!(first, recovery.read(&descriptor, offset, 1).unwrap());
        }
        assert!(
            recovery.visited_tokens <= 128,
            "visited {} tokens",
            recovery.visited_tokens
        );
        recovery.visited_tokens = 0;
        for offset in 0..descriptor.encoded_length {
            let part = recovery.read(&descriptor, offset, 1).unwrap();
            let expected = if offset == 0 {
                9
            } else if offset < 9 {
                (count as u64).to_be_bytes()[(offset - 1) as usize]
            } else {
                0
            };
            assert_eq!(part.bytes, [expected]);
        }
        assert!(recovery.visited_tokens <= descriptor.encoded_length as usize);
        recovery.acknowledge(&descriptor).unwrap();
    }
}

struct Window {
    offset: u64,
    end: u64,
    position: u64,
    bytes: Vec<u8>,
}
impl Window {
    fn write(&mut self, part: &[u8]) {
        let next = self.position + part.len() as u64;
        let start = self.position.max(self.offset);
        let end = next.min(self.end);
        if start < end {
            self.bytes.extend_from_slice(
                &part[(start - self.position) as usize..(end - self.position) as usize],
            );
        }
        self.position = next;
    }
}
