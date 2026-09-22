// jest.package.config.js
//
// Package suites only. The rust-parity suites under
// __tests__/tck/rust-parity/ require the napi addon built by
// `pnpm test:parity` (rust-parity-5-0 CI job) and are excluded here so
// `pnpm test:package` stays runnable without a prebuilt addon.
// NOTE: jest applies testPathIgnorePatterns even to explicit positional
// args, so never run the parity suites through THIS config (it lists
// zero tests); use the base jest.config.js via `pnpm test:parity`.

const base = require('./jest.config.js');

module.exports = {
  ...base,
  testPathIgnorePatterns: [
    ...base.testPathIgnorePatterns,
    '/__tests__/tck/rust-parity/',
  ],
};
