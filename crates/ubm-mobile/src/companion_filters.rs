//! Strict additive selector schema for the Android system chooser. Existing
//! exact-name companion association remains unchanged when no filters are sent.
use serde_json::Value;
use ubm_core::central::canonical_uuid;
use ubm_desktop::DesktopError;

pub(crate) fn validate(text: &str) -> Result<String, DesktopError> {
    let invalid = || crate::wire::invalid("args.filtersJson");
    let mut value: Value = serde_json::from_str(text).map_err(|_| invalid())?;
    let filters = value.as_array_mut().ok_or_else(invalid)?;
    if filters.is_empty() || filters.len() > 16 {
        return Err(invalid());
    }
    for filter in filters {
        let item = filter.as_object_mut().ok_or_else(invalid)?;
        if item.keys().any(|key| {
            ![
                "serviceUuid",
                "namePrefix",
                "companyIdentifier",
                "manufacturerPrefix",
            ]
            .contains(&key.as_str())
        }) {
            return Err(invalid());
        }
        if let Some(service) = item.get_mut("serviceUuid") {
            *service = Value::String(
                canonical_uuid(service.as_str().ok_or_else(invalid)?).map_err(|_| invalid())?,
            );
        }
        if let Some(name) = item.get("namePrefix") {
            let name = name.as_str().ok_or_else(invalid)?;
            if name.is_empty() || name.len() > 1024 {
                return Err(invalid());
            }
        }
        if let Some(company) = item.get("companyIdentifier")
            && company.as_u64().is_none_or(|number| number > 65535)
        {
            return Err(invalid());
        }
        if let Some(prefix) = item.get("manufacturerPrefix") {
            let prefix = prefix.as_array().ok_or_else(invalid)?;
            if !item.contains_key("companyIdentifier")
                || prefix.len() > 512
                || prefix
                    .iter()
                    .any(|byte| byte.as_u64().is_none_or(|number| number > 255))
            {
                return Err(invalid());
            }
        }
    }
    serde_json::to_string(&value).map_err(|_| invalid())
}

#[cfg(test)]
mod tests {
    #[test]
    fn selectors_preserve_or_and_prefix_and_exact_existing_fields() {
        let filters = super::validate(r#"[{"serviceUuid":"180d","namePrefix":"Polar"},{"companyIdentifier":107,"manufacturerPrefix":[0,255]},{}]"#).unwrap();
        assert!(filters.contains("0000180d-0000-1000-8000-00805f9b34fb"));
        assert!(filters.contains("\"namePrefix\":\"Polar\""));
    }
    #[test]
    fn malformed_selectors_never_reach_native() {
        for text in [
            "[]",
            "{}",
            r#"[{"extra":true}]"#,
            r#"[{"companyIdentifier":true}]"#,
            r#"[{"companyIdentifier":65536}]"#,
            r#"[{"manufacturerPrefix":[1]}]"#,
            r#"[{"companyIdentifier":1,"manufacturerPrefix":[-1]}]"#,
            r#"[{"namePrefix":""}]"#,
            r#"[{"serviceUuid":"bad"}]"#,
        ] {
            assert!(super::validate(text).is_err(), "{text}");
        }
    }
}
