// bindings/ubm_build_identity.rs
//
// PR210-18: shared build-script half of the native build identity
// (`ubm-native-build-identity/1`). Each binding's build.rs includes this file
// and calls `ubm_build_identity::emit("<binding>")`. It writes
// `$OUT_DIR/ubm_build_identity.rs`, which the binding crate includes to get
// the identity constants and `ubm_build_identity_json(contract_revision)`.
//
// The two sealed values come from the canonical builders, which compute them
// with scripts/release/native-build-identity.js and pass them to cargo:
//   UBM_BUILD_SOURCE_DIGEST   sha256 over the crate + transitive path deps
//   UBM_BUILD_BINDING_SCHEMA  sha256 over the wrapper-side declarations
// When a variable is unset or empty the value is "unsealed", which the
// runtime identity check always rejects. Any other value must be 64
// lowercase hex characters, or the build fails.

mod ubm_build_identity {
    use std::env;
    use std::fmt::Write as _;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;

    pub const SCHEMA: &str = "ubm-native-build-identity/1";
    const UNSEALED: &str = "unsealed";
    // Appended verbatim to the generated file: the one runtime piece, so the
    // contract revision is the core's compile-time constant, never a copy.
    const JSON_FUNCTION: &str = r#"
/// The frozen `ubm-native-build-identity/1` record for this binary.
/// `contract_revision` is the core's `CONTRACT_REVISION`.
pub fn ubm_build_identity_json(contract_revision: &str) -> String {
    let mut out = String::with_capacity(
        UBM_BUILD_IDENTITY_JSON_PREFIX.len() + UBM_BUILD_IDENTITY_JSON_SUFFIX.len() + contract_revision.len() + 2,
    );
    out.push_str(UBM_BUILD_IDENTITY_JSON_PREFIX);
    out.push('"');
    for character in contract_revision.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            control if (control as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", control as u32));
            }
            other => out.push(other),
        }
    }
    out.push('"');
    out.push_str(UBM_BUILD_IDENTITY_JSON_SUFFIX);
    out
}
"#;
    const SEALED_VARIABLES: [&str; 2] = ["UBM_BUILD_SOURCE_DIGEST", "UBM_BUILD_BINDING_SCHEMA"];

    fn required_env(name: &str) -> String {
        env::var(name)
            .unwrap_or_else(|error| panic!("ubm-build-identity: cargo did not set {name}: {error}"))
    }

    fn sealed_value(name: &str) -> String {
        println!("cargo:rerun-if-env-changed={name}");
        let value = match env::var(name) {
            Ok(value) => value,
            Err(env::VarError::NotPresent) => String::new(),
            Err(error) => panic!("ubm-build-identity: {name} is not valid unicode: {error}"),
        };
        if value.is_empty() || value == UNSEALED {
            return UNSEALED.to_owned();
        }
        let is_digest = value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if !is_digest {
            panic!("ubm-build-identity: {name} must be 64 lowercase hex characters or unset, got {value:?}");
        }
        value
    }

    fn rustc_version() -> String {
        let rustc = required_env("RUSTC");
        let output = Command::new(&rustc)
            .arg("-V")
            .output()
            .unwrap_or_else(|error| panic!("ubm-build-identity: cannot run {rustc} -V: {error}"));
        if !output.status.success() {
            panic!(
                "ubm-build-identity: {rustc} -V exited with {}",
                output.status
            );
        }
        String::from_utf8(output.stdout)
            .unwrap_or_else(|error| {
                panic!("ubm-build-identity: {rustc} -V printed non-UTF-8: {error}")
            })
            .trim()
            .to_owned()
    }

    fn enabled_features() -> Vec<String> {
        let mut features: Vec<String> = env::vars()
            .filter_map(|(key, _)| {
                key.strip_prefix("CARGO_FEATURE_")
                    .map(str::to_ascii_lowercase)
            })
            .collect();
        features.sort();
        features
    }

    fn json_string(value: &str) -> String {
        let mut out = String::with_capacity(value.len() + 2);
        out.push('"');
        for character in value.chars() {
            match character {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                control if (control as u32) < 0x20 => {
                    write!(out, "\\u{:04x}", control as u32).expect("write to String");
                }
                other => out.push(other),
            }
        }
        out.push('"');
        out
    }

    pub fn emit(binding: &str) {
        let source_digest = sealed_value(SEALED_VARIABLES[0]);
        let binding_schema = sealed_value(SEALED_VARIABLES[1]);
        let target = required_env("TARGET");
        let profile = required_env("PROFILE");
        let features = enabled_features();
        let rustc = rustc_version();

        let features_json = features
            .iter()
            .map(|feature| json_string(feature))
            .collect::<Vec<_>>()
            .join(",");
        let prefix = format!(
            "{{\"schema\":{},\"binding\":{},\"contractRevision\":",
            json_string(SCHEMA),
            json_string(binding)
        );
        let suffix = format!(
            ",\"sourceDigest\":{},\"bindingSchema\":{},\"target\":{},\"profile\":{},\"features\":[{}],\"rustc\":{}}}",
            json_string(&source_digest),
            json_string(&binding_schema),
            json_string(&target),
            json_string(&profile),
            features_json,
            json_string(&rustc)
        );
        let features_rust = features
            .iter()
            .map(|feature| format!("{feature:?}"))
            .collect::<Vec<_>>()
            .join(", ");

        let generated = format!(
            "// @generated by bindings/ubm_build_identity.rs (build.rs). Do not edit.\n\
             #[allow(dead_code)]\n\
             pub const UBM_BUILD_IDENTITY_SCHEMA: &str = {SCHEMA:?};\n\
             #[allow(dead_code)]\n\
             pub const UBM_BUILD_BINDING: &str = {binding:?};\n\
             #[allow(dead_code)]\n\
             pub const UBM_BUILD_SOURCE_DIGEST: &str = {source_digest:?};\n\
             #[allow(dead_code)]\n\
             pub const UBM_BUILD_BINDING_SCHEMA: &str = {binding_schema:?};\n\
             #[allow(dead_code)]\n\
             pub const UBM_BUILD_TARGET: &str = {target:?};\n\
             #[allow(dead_code)]\n\
             pub const UBM_BUILD_PROFILE: &str = {profile:?};\n\
             #[allow(dead_code)]\n\
             pub const UBM_BUILD_FEATURES: &[&str] = &[{features_rust}];\n\
             #[allow(dead_code)]\n\
             pub const UBM_BUILD_RUSTC: &str = {rustc:?};\n\
             const UBM_BUILD_IDENTITY_JSON_PREFIX: &str = {prefix:?};\n\
             const UBM_BUILD_IDENTITY_JSON_SUFFIX: &str = {suffix:?};\n\
             {JSON_FUNCTION}"
        );
        let out_dir = PathBuf::from(required_env("OUT_DIR"));
        let target_file = out_dir.join("ubm_build_identity.rs");
        fs::write(&target_file, generated).unwrap_or_else(|error| {
            panic!(
                "ubm-build-identity: cannot write {}: {error}",
                target_file.display()
            )
        });
    }
}
