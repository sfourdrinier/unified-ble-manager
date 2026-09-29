import assert from 'node:assert/strict'
import { readFileSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { execFileSync, spawnSync } from 'node:child_process'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'
import test from 'node:test'

const source = new URL('../../../native/ios/ReferenceContinuationModule.swift', import.meta.url)
const registration = new URL('../../../native/ios/ReferenceContinuationModule.m', import.meta.url)

test('Apple app transport executes against controlled native and React boundaries', { skip: process.platform !== 'darwin' }, () => {
  const temporary = mkdtempSync(join(tmpdir(), 'ubm-apple-reference-test-'))
  const fixtures = fileURLToPath(new URL('../../../native/ios-test/', import.meta.url))
  const swift = args => execFileSync('xcrun', ['swiftc', ...args], { encoding: 'utf8' })
  try {
    for (const name of ['React', 'BlePlx']) {
      swift(['-emit-library', '-emit-module', '-module-name', name, join(fixtures, `${name}.swift`),
        '-emit-module-path', join(temporary, `${name}.swiftmodule`), '-o', join(temporary, `lib${name}.dylib`)])
    }
    const executable = join(temporary, 'harness')
    swift(['-I', temporary, '-L', temporary, '-lReact', '-lBlePlx', '-Xlinker', '-rpath', '-Xlinker', temporary,
      fileURLToPath(source), join(fixtures, 'Harness.swift'), '-o', executable])
    const execution = spawnSync(executable, [], { encoding: 'utf8' })
    assert.equal(execution.status, 0, execution.stderr)
    assert.match(execution.stderr, /duplicate native completion ignored/)
    const output = execution.stdout
    assert.match(output, /bounded admission and recovery passed/)
    const envelopes = output.split('\n').filter(line => /^(invalid|busy)-envelope=/.test(line))
      .map(line => line.slice(line.indexOf('=') + 1)).join('\n') + '\n'
    // Decode the Swift adapter's actual emitted bytes through today's shared
    // TypeScript decoder, not a manually duplicated envelope fixture.
    execFileSync(process.execPath, [
      fileURLToPath(new URL('../../../native/validate-envelopes.cjs', import.meta.url)), '10'
    ], { input: envelopes, encoding: 'utf8' })
    if (process.env.UBM_APPLE_REFERENCE_ENVELOPES_OUTPUT) {
      writeFileSync(process.env.UBM_APPLE_REFERENCE_ENVELOPES_OUTPUT, envelopes)
    }
  } finally { rmSync(temporary, { recursive: true, force: true }) }
})

test('Apple app-only adapter registers the shared Android bridge signature', () => {
  const text = readFileSync(registration, 'utf8')
  assert.match(text, /RCT_EXTERN_MODULE\(UBMReferenceContinuation, NSObject\)/)
  for (const field of ['operation', 'peer', 'declarationJson', 'token', 'maxItems', 'maxBytes', 'resolve', 'reject']) {
    assert.ok(text.includes(field), `missing bridge argument ${field}`)
  }
})

test('Apple warm controls retain native ownership and bounded off-main admission', () => {
  const text = readFileSync(source, 'utf8')
  for (const method of ['executeNativeContinuation', 'describeNativeContinuation', 'prepareNativeContinuationClaim', 'acknowledgeNativeContinuationClaim']) {
    assert.ok(text.includes(method), `missing native control ${method}`)
  }
  assert.match(text, /UnifiedBleRustCoreSessions.shared/)
  assert.match(text, /DispatchQueue\(label:/)
  assert.match(text, /DispatchSemaphore\(value: 16\)/)
  assert.match(text, /65536/)
  assert.match(text, /4194304/)
  assert.match(text, /2048/)
  assert.match(text, /256/)
  assert.doesNotMatch(text, /continueRestoredPeer|finishContinuation|UserDefaults|shutdown|invalidate/)
})
