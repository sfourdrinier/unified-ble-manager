//! Exact WinRT PHY flags; no preference or all-false default is an observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionPhyKind { Le1M, Le2M, LeCoded }
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionPhy { pub tx: ConnectionPhyKind, pub rx: ConnectionPhyKind }
pub fn direction(one_m: bool, two_m: bool, coded: bool) -> crate::Result<ConnectionPhyKind> {
    match (one_m, two_m, coded) {
        (true, false, false) => Ok(ConnectionPhyKind::Le1M),
        (false, true, false) => Ok(ConnectionPhyKind::Le2M),
        (false, false, true) => Ok(ConnectionPhyKind::LeCoded),
        (false, false, false) => Err(crate::Error::Platform(crate::PlatformError::new("winrt", "connection-phy-disconnected", "GetConnectionPhy returned invalid all-false flags"))),
        _ => Err(crate::Error::Platform(crate::PlatformError::new("winrt", "connection-phy-malformed", "GetConnectionPhy returned contradictory direction flags"))),
    }
}
#[cfg(test)] mod tests {
    use super::*;
    #[test] fn exact_direction_flags_never_invent_a_phy() {
        assert_eq!(direction(true,false,false).unwrap(), ConnectionPhyKind::Le1M);
        assert_eq!(direction(false,true,false).unwrap(), ConnectionPhyKind::Le2M);
        assert_eq!(direction(false,false,true).unwrap(), ConnectionPhyKind::LeCoded);
        for flags in [(false,false,false),(true,true,false),(false,true,true),(true,false,true),(true,true,true)] {
            assert!(direction(flags.0,flags.1,flags.2).is_err());
        }
    }
}
