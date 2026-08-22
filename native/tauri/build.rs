#[path = "src/security_scope.rs"]
mod security_scope;

const COMMANDS: &[&str] = &["invoke"];

fn main() {
    tauri_plugin::Builder::new(COMMANDS)
        .global_scope_schema(schemars::schema_for!(security_scope::SecurityPermission))
        .build();
}
