//! The WinRT callback's verdict mapping, independent of the COM boundary.

pub fn callback_answer(
    read: impl FnOnce() -> crate::Result<crate::api::ConnectionParameters>,
) -> Result<crate::api::ConnectionParameters, crate::PlatformError> {
    read().map_err(|error| match error {
        crate::Error::Platform(platform) => platform,
        other => crate::PlatformError::new(
            "winrt",
            "connection-parameter-getter-failed",
            other.to_string(),
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_getter_failure_keeps_the_original_hresult_and_metadata() {
        let source = crate::PlatformError::new("winrt", "0x80070005", "getter access denied")
            .with("hresult", "0x80070005")
            .with("operation", "GetConnectionParameters");
        let result = callback_answer(|| Err(crate::Error::Platform(source.clone())));
        assert_eq!(result, Err(source));
    }

    #[test]
    fn callback_success_is_the_getter_answer_and_nonplatform_failure_stays_explicit() {
        let observed = crate::api::ConnectionParameters {
            interval_us: 60_000,
            latency: 4,
            supervision_timeout_us: 6_000_000,
        };
        assert_eq!(callback_answer(|| Ok(observed)), Ok(observed));
        let error =
            callback_answer(|| Err(crate::Error::Other("getter source disappeared".into())))
                .unwrap_err();
        assert_eq!(error.code, "connection-parameter-getter-failed");
        assert!(error.message.contains("getter source disappeared"));
    }
}
