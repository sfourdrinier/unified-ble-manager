/** Cargo resolves paths from the consuming src-tauri/Cargo.toml. */
export function tauriCargoRecipe(packageRoot = '../node_modules/unified-ble-manager'): string {
  const root = packageRoot.replace(/\/$/, '')
  return [
    '[dependencies]',
    'tauri = { version = "2", features = [] }',
    `tauri-plugin-unified-ble-manager = { path = "${root}/native/tauri" }`,
    '',
    '# Cargo reads patches only from the consuming workspace root.',
    '[patch.crates-io]',
    `btleplug = { path = "${root}/vendor/btleplug" }`,
    `bluez-async = { path = "${root}/vendor/bluez-async" }`,
    ''
  ].join('\n')
}
