// contracts/jest.config.cjs — explicit U1 contract test config.
// Scoped to contracts/ only; no package.json edits, no lane global setup.
const path = require('node:path');

module.exports = {
  rootDir: __dirname,
  roots: ['<rootDir>/__tests__'],
  testEnvironment: 'node',
  testMatch: ['**/*.test.ts'],
  transform: {
    '^.+\\.[tj]sx?$': [
      'babel-jest',
      { configFile: path.join(__dirname, '..', 'babel.config.js') },
    ],
  },
};
