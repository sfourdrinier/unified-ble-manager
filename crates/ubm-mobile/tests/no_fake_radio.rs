//! The production mobile owner has no path to the staged/fake radio.

use std::process::Command;

#[test]
fn normal_dependency_tree_excludes_fake_radio() {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let output = Command::new(cargo)
        .args([
            "tree",
            "-p",
            "ubm-mobile",
            "-e",
            "normal",
            "--prefix",
            "none",
            "--offline",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("cargo tree runs");
    assert!(
        output.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let tree = String::from_utf8_lossy(&output.stdout);
    assert!(
        tree.contains("ubm-desktop"),
        "tree lists the shared central:\n{tree}"
    );
    for forbidden in ["ubm-fake-radio", "btleplug"] {
        assert!(
            !tree.lines().any(|line| line.starts_with(forbidden)),
            "{forbidden} reachable from ubm-mobile:\n{tree}"
        );
    }
}
