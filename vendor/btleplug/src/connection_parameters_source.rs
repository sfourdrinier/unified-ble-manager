//! The WinRT callback's verdict mapping, independent of the COM boundary.

/// Interpret the public WinRT getter's own answer, before transport projection.
/// Microsoft defines an all-zero result as a disconnected device. Zero latency
/// alone is valid and partial zeros remain subject to shared sample validation.
pub fn parameter_answer(
    interval: u16,
    latency: u16,
    timeout: u16,
) -> crate::Result<crate::api::ConnectionParameters> {
    if interval == 0 && latency == 0 && timeout == 0 {
        return Err(crate::Error::Platform(
            crate::PlatformError::new(
                "winrt",
                "connection-parameters-disconnected",
                "GetConnectionParameters returned the disconnected all-zero result",
            )
            .with("connectionInterval", interval.to_string())
            .with("connectionLatency", latency.to_string())
            .with("linkTimeout", timeout.to_string()),
        ));
    }
    Ok(crate::api::ConnectionParameters {
        interval_us: u32::from(interval) * 1250,
        latency,
        supervision_timeout_us: u32::from(timeout) * 10_000,
    })
}

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
