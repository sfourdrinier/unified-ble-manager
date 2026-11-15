// jest.config.js

module.exports = {
  roots: ['<rootDir>/__tests__'],
  globalSetup: '<rootDir>/scripts/ci/jest-zero-diagnostic-global-setup.js',
  setupFilesAfterEnv: ['<rootDir>/__tests__/helpers/zero-diagnostic-guard.js'],
  testResultsProcessor: '<rootDir>/scripts/ci/jest-zero-diagnostic-results.js',
  // Shared fixtures under helpers/ export modules only — not test suites (F086/F087).
  testPathIgnorePatterns: [
    '/node_modules/',
    '/__tests__/helpers/',
    '/__tests__/electron/helpers/',
    '/__tests__/docs-recipes/',
    '/__tests__/fixtures/tauri-bundler/',
    '/__tests__/fixtures/discovery-compat\\.ts$',
    '/__tests__/backend-contract/fixtures/',
    '/__tests__/package-surface/fixtures/',
    // These are browser-only/manual fixture programs, or modules imported by
    // their adjacent `.test.*` suites. The React Native preset also discovers
    // arbitrary JavaScript inside `__tests__`, so list them here rather than
    // broadening the exclusion to the whole web test directory.
    '/__tests__/web/web-(?:active-upgrade|clear-admission-partition|clear-proof-partition|clear-proof-staged|clear-proof-v2|cold-recovery|marker-ledger-transactions-522|marker-ledger-upgrade-522|recovery-pages-522|terminal-evidence)-browser-fixture\\.(?:js|ts)$',
    '/__tests__/web/web-(?:authority-reservation|clear-admission-locks|continuation-recording-controller|marker-ledger-database-522|ordered-recording-journal|recovery-522)-helper\\.js$',
    '/__tests__/web/web-real-indexeddb-upgrade-fixture\\.ts$'
  ],
  preset: '@react-native/jest-preset',
  transformIgnorePatterns: ['node_modules/(?!.*react-native.*)'],
  moduleNameMapper: {
    // The React Native Jest resolver selects YAML's browser ESM entrypoint for
    // CommonJS release-policy tests. Pin the package's CommonJS entrypoint for
    // Jest only; production resolution remains package-managed.
    '^yaml$': '<rootDir>/node_modules/yaml/dist/index.js',
    // Phase 0+: resolve package identity names to this repo during unit tests
    '^unified-ble-manager/web$': '<rootDir>/src/web.ts',
    '^unified-ble-manager/react-native$': '<rootDir>/src/react-native.ts',
    '^unified-ble-manager/node/corebluetooth$': '<rootDir>/src/node-corebluetooth.ts',
    '^unified-ble-manager/node/bluez$': '<rootDir>/src/node-bluez.ts',
    '^unified-ble-manager/profiles/(.*)$': '<rootDir>/src/profiles/$1.ts',
    '^unified-ble-manager/backend-sdk$': '<rootDir>/src/backend-sdk.ts',
    '^unified-ble-manager/cli$': '<rootDir>/src/cli.ts',
    '^unified-ble-manager/testing$': '<rootDir>/src/testing.ts',
    '^unified-ble-manager/node/winrt$': '<rootDir>/src/node-winrt.ts',
    '^unified-ble-manager/electron/main$': '<rootDir>/src/electron-main.ts',
    '^unified-ble-manager/electron/renderer$': '<rootDir>/src/electron-renderer.ts',
    '^unified-ble-manager$': '<rootDir>/src/index.ts'
  }
}
