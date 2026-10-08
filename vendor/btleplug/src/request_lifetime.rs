//! A closeable native request remains owned until close succeeds. Admission
//! must retire the previous request before creating another, bounding debt.
pub(crate) struct RequestLifetime<T> {
    request: Option<T>,
}

impl<T> Default for RequestLifetime<T> {
    fn default() -> Self {
        Self { request: None }
    }
}

impl<T> RequestLifetime<T> {
    pub(crate) fn retain(&mut self, request: T) {
        assert!(
            self.request.is_none(),
            "previous native request is still owned"
        );
        self.request = Some(request);
    }

    pub(crate) fn close<E>(&mut self, close: impl FnOnce(&T) -> Result<(), E>) -> Result<(), E> {
        if let Some(request) = self.request.as_ref() {
            close(request)?;
            self.request = None;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::RequestLifetime;

    #[test]
    fn failed_close_retains_the_exact_request_for_retry() {
        let mut owner = RequestLifetime::default();
        owner.retain(42);
        assert_eq!(
            owner.close(|request| {
                assert_eq!(*request, 42);
                Err("close-failed")
            }),
            Err("close-failed")
        );
        assert_eq!(
            owner.close(|request| {
                assert_eq!(*request, 42);
                Ok::<_, &str>(())
            }),
            Ok(())
        );
        owner.retain(43);
        assert_eq!(
            owner.close(|request| {
                assert_eq!(*request, 43);
                Ok::<_, &str>(())
            }),
            Ok(())
        );
        assert_eq!(owner.close(|_| panic!("already closed")), Ok::<_, &str>(()));
    }
}
