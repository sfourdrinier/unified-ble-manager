//! A daemon generation cannot be repaired by a later owner reappearing.

use std::sync::Mutex;

#[derive(Default)]
pub struct DaemonLifetime(Mutex<(Option<String>, bool)>);

impl DaemonLifetime {
    pub fn owner_changed(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).1 = true;
    }
    pub fn bind(&self, owner: &str) -> Result<(), String> {
        let mut state = self.0.lock().map_err(|_| "daemon lifetime lock poisoned")?;
        if state.1 || owner.is_empty() {
            return Err("BlueZ daemon generation lost before registration".into());
        }
        if state.0.is_some() {
            return Err("BlueZ daemon generation already bound".into());
        }
        state.0 = Some(owner.to_owned());
        Ok(())
    }
    pub fn check(&self, owner: &str) -> Result<(), String> {
        let mut state = self.0.lock().map_err(|_| "daemon lifetime lock poisoned")?;
        if state.0.as_deref() != Some(owner) {
            state.1 = true;
        }
        if state.1 {
            Err("BlueZ daemon generation lost; restart simulator".into())
        } else {
            Ok(())
        }
    }
    pub fn admit(&self) -> Result<(), String> {
        let state = self.0.lock().map_err(|_| "daemon lifetime lock poisoned")?;
        if state.1 || state.0.is_none() {
            Err("BlueZ daemon generation unavailable; restart simulator".into())
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::DaemonLifetime;

    #[test]
    fn loss_before_baseline_and_rapid_replacement_never_reopen_admission() {
        let lifetime = DaemonLifetime::default();
        lifetime.owner_changed();
        assert!(lifetime.bind(":1.2").is_err());
        assert!(lifetime.check(":1.2").is_err());
    }

    #[test]
    fn exact_owner_is_required_and_replacement_is_sticky() {
        let lifetime = DaemonLifetime::default();
        assert!(lifetime.admit().is_err());
        lifetime.bind(":1.1").unwrap();
        lifetime.check(":1.1").unwrap();
        assert!(lifetime.check(":1.2").is_err());
        assert!(lifetime.check(":1.1").is_err());
    }

    #[test]
    fn loss_during_registration_and_late_loss_are_idempotent() {
        let lifetime = DaemonLifetime::default();
        lifetime.bind(":1.1").unwrap();
        lifetime.owner_changed();
        lifetime.owner_changed();
        assert!(lifetime.admit().is_err());
        assert!(lifetime.bind(":1.3").is_err());
    }
}
