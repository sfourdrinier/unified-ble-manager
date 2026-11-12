//! Incremental ownership of WinRT event registrations.
pub(super) fn retain_failed_drop<T, E>(
    owner: &mut T,
    cleanup: impl FnOnce(&mut T) -> Result<(), E>,
    retain: impl FnOnce(&mut T, E),
) {
    if let Err(error) = cleanup(owner) {
        retain(owner, error);
    }
}

pub(super) fn close_once<E>(
    flag: &mut bool,
    close: impl FnOnce() -> Result<(), E>,
) -> Result<(), E> {
    if !*flag {
        close()?;
        *flag = true;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Event {
    Connection,
    Pdu,
    Parameters,
    Services,
}

#[derive(Default)]
pub(super) struct EventTokens {
    connection: Option<i64>,
    pdu: Option<i64>,
    parameters: Option<i64>,
    services: Option<i64>,
}

impl EventTokens {
    fn slot(&mut self, event: Event) -> &mut Option<i64> {
        match event {
            Event::Connection => &mut self.connection,
            Event::Pdu => &mut self.pdu,
            Event::Parameters => &mut self.parameters,
            Event::Services => &mut self.services,
        }
    }

    pub(super) fn record(&mut self, event: Event, token: i64) {
        let slot = self.slot(event);
        assert!(slot.is_none(), "event registration already owned");
        *slot = Some(token);
    }

    pub(super) fn remove<E>(
        &mut self,
        event: Event,
        remove: impl FnOnce(i64) -> Result<(), E>,
    ) -> Result<(), E> {
        let slot = self.slot(event);
        if let Some(token) = *slot {
            remove(token)?;
            *slot = None;
        }
        Ok(())
    }

    pub(super) fn remove_all<E>(
        &mut self,
        mut remove: impl FnMut(Event, i64) -> Result<(), E>,
    ) -> Result<(), E> {
        for event in [
            Event::Pdu,
            Event::Parameters,
            Event::Services,
            Event::Connection,
        ] {
            self.remove(event, |token| remove(event, token))?;
        }
        Ok(())
    }
}
