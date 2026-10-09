// tool/h10-sim/src/psftp.rs
//! Minimal RFC76 framing for the advertised Polar PSFTP service.
//!
//! This module deliberately stops at protocol acknowledgement. It does not
//! interpret a request as a file, recording, or other device object. Every
//! completed H2D request receives the protocol-level `NOT_IMPLEMENTED(201)`
//! D2H response, while malformed or incomplete traffic receives no response.

use crate::radio::OwnedNotifyTarget;
use uuid::Uuid;

/// Polar PSFTP request/response traffic uses FEEE CHAR_51 (RFC76/RFC77).
pub fn wire_characteristic() -> Uuid {
    Uuid::parse_str(crate::gatt_spec::feee::CHAR_51).expect("valid PSFTP CHAR_51 UUID")
}

/// Returns whether a write is a PSFTP request on the bidirectional wire.
pub fn is_wire_request(characteristic: Uuid) -> bool {
    characteristic == wire_characteristic()
}

/// PSFTP responses return on the same FEEE CHAR_51 channel as requests.
pub fn wire_response_characteristic() -> Uuid {
    wire_characteristic()
}

/// Maximum assembled H2D payload retained by one connection/subscription.
pub const MAX_MESSAGE_BYTES: usize = 4096;

const NEXT_BIT: u8 = 0x01;
const STATUS_MASK: u8 = 0x06;
const SEQUENCE_MASK: u8 = 0xF0;
const STATUS_LAST: u8 = 0x02;
const STATUS_MORE: u8 = 0x06;

/// A malformed RFC76 stream is discarded and must be restarted at sequence 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    EmptyFrame,
    EmptyFragment,
    FirstFrame,
    Sequence { expected: u8, got: u8 },
    InvalidStatus(u8),
    MessageTooLarge,
    Ownership,
}

#[derive(Debug, Default)]
pub struct Session {
    payload: Vec<u8>,
    expected_sequence: u8,
    started: bool,
    owner: Option<OwnedNotifyTarget>,
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one ATT H2D write fragment.
    ///
    /// `Some(payload)` is returned exactly once, on a valid LAST frame. An
    /// error resets the stream so a later valid request cannot inherit stale
    /// bytes or sequence state.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn ingest(&mut self, frame: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        self.ingest_inner(frame)
    }

    /// Adds a fragment only for the peer and D2H subscription generation that
    /// admitted the write. A mismatch resets before frame parsing.
    pub fn ingest_for(
        &mut self,
        owner: &OwnedNotifyTarget,
        frame: &[u8],
    ) -> Result<Option<Vec<u8>>, Error> {
        if self.owner.as_ref().is_some_and(|current| current != owner) {
            self.reset();
            return Err(Error::Ownership);
        }
        self.owner = Some(owner.clone());
        self.ingest_inner(frame)
    }

    fn ingest_inner(&mut self, frame: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        let Some(&header) = frame.first() else {
            self.reset();
            return Err(Error::EmptyFrame);
        };
        let next = header & NEXT_BIT;
        let status = header & STATUS_MASK;
        let sequence = (header & SEQUENCE_MASK) >> 4;

        if !self.started {
            if next != 0 || sequence != 0 {
                self.reset();
                return Err(Error::FirstFrame);
            }
        } else if next != 1 {
            self.reset();
            return Err(Error::FirstFrame);
        } else if sequence != self.expected_sequence {
            let expected = self.expected_sequence;
            self.reset();
            return Err(Error::Sequence {
                expected,
                got: sequence,
            });
        }

        if status != STATUS_MORE && status != STATUS_LAST {
            self.reset();
            return Err(Error::InvalidStatus(status >> 1));
        }

        let incoming = &frame[1..];
        if status == STATUS_MORE && incoming.is_empty() {
            self.reset();
            return Err(Error::EmptyFragment);
        }
        if self.payload.len().saturating_add(incoming.len()) > MAX_MESSAGE_BYTES {
            self.reset();
            return Err(Error::MessageTooLarge);
        }
        self.payload.extend_from_slice(incoming);

        if status == STATUS_LAST {
            let payload = std::mem::take(&mut self.payload);
            self.started = false;
            self.expected_sequence = 0;
            self.owner = None;
            Ok(Some(payload))
        } else {
            self.started = true;
            self.expected_sequence = (sequence + 1) & 0x0F;
            Ok(None)
        }
    }

    /// Retires all parser state at a connection/subscription boundary.
    pub fn reset(&mut self) {
        self.payload.clear();
        self.expected_sequence = 0;
        self.started = false;
        self.owner = None;
    }
}

/// RFC76 D2H error frame for `PbPFtpError.NOT_IMPLEMENTED = 201`.
pub const fn not_implemented_response() -> [u8; 3] {
    [0x00, 0xC9, 0x00]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::radio::OwnedNotifyTarget;

    #[test]
    fn completed_single_frame_request_gets_not_implemented_response() {
        let mut session = Session::new();
        assert_eq!(session.ingest(&[0x02, 0x10]), Ok(Some(vec![0x10])));
        assert_eq!(not_implemented_response(), [0x00, 0xC9, 0x00]);
    }

    #[test]
    fn fragmented_request_completes_once_at_last_frame() {
        let mut session = Session::new();
        assert_eq!(session.ingest(&[0x06, 0xAA]), Ok(None));
        assert_eq!(session.ingest(&[0x17, 0xBB, 0xCC]), Ok(None));
        assert_eq!(
            session.ingest(&[0x23, 0xDD]),
            Ok(Some(vec![0xAA, 0xBB, 0xCC, 0xDD]))
        );
    }

    #[test]
    fn sequence_gap_discards_partial_request_without_response() {
        let mut session = Session::new();
        assert_eq!(session.ingest(&[0x06, 0xAA]), Ok(None));
        assert_eq!(
            session.ingest(&[0x27, 0xBB]),
            Err(Error::Sequence {
                expected: 1,
                got: 2
            })
        );
        assert_eq!(session.ingest(&[0x02, 0xCC]), Ok(Some(vec![0xCC])));
    }

    #[test]
    fn reset_discards_old_fragments_before_successor_connection() {
        let mut session = Session::new();
        assert_eq!(session.ingest(&[0x06, 0xAA]), Ok(None));
        session.reset();
        assert_eq!(session.ingest(&[0x23, 0xBB]), Err(Error::FirstFrame));
        assert_eq!(session.ingest(&[0x02, 0xCC]), Ok(Some(vec![0xCC])));
    }

    #[test]
    fn oversized_message_is_rejected_and_reset() {
        let mut session = Session::new();
        let payload = vec![0xAA; MAX_MESSAGE_BYTES + 1];
        let mut frame = vec![0x06];
        frame.extend_from_slice(&payload);
        assert_eq!(session.ingest(&frame), Err(Error::MessageTooLarge));
        assert_eq!(session.ingest(&[0x02, 0xCC]), Ok(Some(vec![0xCC])));
    }

    #[test]
    fn empty_more_frame_is_rejected_and_resets_the_request() {
        let mut session = Session::new();
        assert_eq!(session.ingest(&[0x06]), Err(Error::EmptyFragment));
        assert_eq!(session.ingest(&[0x02, 0xCC]), Ok(Some(vec![0xCC])));
    }

    #[test]
    fn fragments_from_a_foreign_owner_cannot_complete_the_request() {
        let mut session = Session::new();
        let owner_a = OwnedNotifyTarget::new("AA:AA:AA:AA:AA:AA", 7);
        let owner_b = OwnedNotifyTarget::new("BB:BB:BB:BB:BB:BB", 8);

        assert_eq!(session.ingest_for(&owner_a, &[0x06, 0xAA]), Ok(None));
        assert_eq!(
            session.ingest_for(&owner_b, &[0x12, 0xBB]),
            Err(Error::Ownership)
        );
        assert_eq!(
            session.ingest_for(&owner_a, &[0x22, 0xCC]),
            Err(Error::FirstFrame)
        );
        assert_eq!(
            session.ingest_for(&owner_a, &[0x02, 0xDD]),
            Ok(Some(vec![0xDD]))
        );
    }

    #[test]
    fn stale_generation_is_rejected_before_parsing() {
        let mut session = Session::new();
        let old = OwnedNotifyTarget::new("AA:AA:AA:AA:AA:AA", 7);
        let current = OwnedNotifyTarget::new("AA:AA:AA:AA:AA:AA", 8);

        assert_eq!(session.ingest_for(&old, &[0x06, 0xAA]), Ok(None));
        assert_eq!(
            session.ingest_for(&current, &[0x02, 0xBB]),
            Err(Error::Ownership)
        );
        assert_eq!(
            session.ingest_for(&old, &[0x22, 0xCC]),
            Err(Error::FirstFrame)
        );
    }

    #[test]
    fn same_owner_completes_once_with_one_response_payload() {
        let mut session = Session::new();
        let owner = OwnedNotifyTarget::new("AA:AA:AA:AA:AA:AA", 7);

        assert_eq!(
            session.ingest_for(&owner, &[0x02, 0x10]),
            Ok(Some(vec![0x10]))
        );
        assert_eq!(not_implemented_response(), [0x00, 0xC9, 0x00]);
        assert_eq!(
            session.ingest_for(&owner, &[0x02, 0x11]),
            Ok(Some(vec![0x11]))
        );
    }
}
