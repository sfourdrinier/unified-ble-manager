'use strict'
// Shared executable validation of actual Swift/Kotlin wrapper output.
const fs = require('node:fs')
const assert = require('node:assert/strict')
const ts = require('typescript')
require.extensions['.ts'] = (module, filename) => module._compile(ts.transpileModule(fs.readFileSync(filename, 'utf8'), {
  compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 }
}).outputText, filename)
const { decodeNativeContinuationEnvelope } = require('../../src/core/native-continuation-envelope.ts')
const input = process.argv[2] === '--android-xml'
  ? [...fs.readFileSync(process.argv[3], 'utf8').matchAll(/reference-envelope=([^\r\n]+)/g)].map(match => match[1]).join('\n')
  : fs.readFileSync(0, 'utf8')
const expectedCount = Number(process.argv[process.argv[2] === '--android-xml' ? 4 : 2])
const rows = input.trim().split('\n')
assert.ok(Number.isSafeInteger(expectedCount) && expectedCount > 0)
assert.equal(rows.length, expectedCount, 'missing actual wrapper failure output')
for (const row of rows) {
  const expected = JSON.parse(row)
  let actual
  try { decodeNativeContinuationEnvelope(row, 'reference-native') } catch (error) { actual = error.normalized }
  assert.ok(actual, 'failure must decode as a typed error')
  for (const key of ['code', 'domain', 'operation']) assert.equal(actual[key], expected.error[key])
  assert.equal(actual.retryability, expected.retryability)
  if (expected.error.platform != null) {
    assert.equal(actual.platform.domain, expected.error.platform.domain)
    assert.equal(actual.platform.code, expected.error.platform.code)
    assert.deepEqual(actual.platform.metadata, expected.error.platform.metadata)
  }
}
process.stdout.write(`canonical wrapper envelopes: ${rows.length} passed\n`)
