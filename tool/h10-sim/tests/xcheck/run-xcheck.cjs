'use strict'
// tool/h10-sim/tests/xcheck/run-xcheck.cjs
//
// Cross-checks the Rust encoder output against the repository's own TypeScript
// parsers: every vector emitted by `h10-sim --emit-test-vectors` is decoded
// with examples-shared/driver/polar-pmd.ts and src/profiles/heart-rate.ts.
// Any mismatch (or any parser throw) fails loudly with a non-zero exit.
//
// Run from the repository root:
//   node tool/h10-sim/tests/xcheck/run-xcheck.cjs
//
// The script compiles the parsers with the repo's TypeScript into a temp dir,
// so it always checks the current sources, never a stale copy.

const { execFileSync } = require('child_process')
const fs = require('fs')
const os = require('os')
const path = require('path')

const root = path.resolve(__dirname, '..', '..', '..', '..')
const simDir = path.join(root, 'tool', 'h10-sim')

function fail(message) {
  console.error(`xcheck FAILED: ${message}`)
  process.exit(1)
}

function check(name, actual, expected) {
  const a = JSON.stringify(actual)
  const e = JSON.stringify(expected)
  if (a !== e) fail(`${name}: got ${a}, expected ${e}`)
  console.log(`ok: ${name}`)
}

function checkClose(name, actual, expected, tolerance) {
  if (Math.abs(actual - expected) > tolerance) {
    fail(`${name}: got ${actual}, expected ${expected} ± ${tolerance}`)
  }
  console.log(`ok: ${name}`)
}

// 1. Emit vectors from the Rust encoders.
let vectors
try {
  const out = execFileSync('cargo', ['run', '--quiet', '--', '--emit-test-vectors'], {
    cwd: simDir,
    encoding: 'utf8',
    stdio: ['ignore', 'pipe', 'pipe'],
    env: {
      ...process.env,
      CARGO_HOME: process.env.CARGO_HOME || path.join(os.tmpdir(), 'h10cargo'),
      CARGO_TARGET_DIR: process.env.CARGO_TARGET_DIR || path.join(os.tmpdir(), 'h10sim-target')
    }
  })
  vectors = JSON.parse(out)
} catch (error) {
  fail(`could not emit vectors: ${error.message}`)
}

// 2. Compile the repo's own parsers to CommonJS in a temp dir.
const dist = fs.mkdtempSync(path.join(os.tmpdir(), 'h10xcheck-'))
try {
  execFileSync(
    path.join(root, 'node_modules', '.bin', 'tsc'),
    [
      'examples-shared/driver/polar-pmd.ts',
      'src/profiles/heart-rate.ts',
      '--outDir',
      dist,
      '--module',
      'commonjs',
      '--target',
      'es2022',
      '--moduleResolution',
      'node',
      '--skipLibCheck',
      '--declaration',
      'false',
      '--sourceMap',
      'false'
    ],
    { cwd: root, stdio: 'pipe' }
  )
} catch (error) {
  fail(`could not compile parsers: ${error.stdout || error.message}`)
}

const pmd = require(path.join(dist, 'examples-shared', 'driver', 'polar-pmd.js'))
const hr = require(path.join(dist, 'src', 'profiles', 'heart-rate.js'))

// 3. Heart-rate vectors through the SIG profile parser.
for (const vector of vectors.hr_measurements) {
  const parsed = hr.parseHeartRateMeasurement(Uint8Array.from(vector.bytes))
  check(`hr bpm ${vector.bpm}`, parsed.beatsPerMinute, vector.bpm)
  check(`hr contact ${vector.bpm}`, parsed.contact, 'detected')
  if (parsed.rrIntervalsSeconds.length !== vector.rr_s.length) {
    fail(`hr rr count: got ${parsed.rrIntervalsSeconds.length}, expected ${vector.rr_s.length}`)
  }
  vector.rr_s.forEach((expected, index) => {
    checkClose(`hr rr[${index}]`, parsed.rrIntervalsSeconds[index], expected, 1 / 1024 + 1e-9)
  })
  console.log(`ok: hr vector bpm=${vector.bpm}`)
}
const noRr = hr.parseHeartRateMeasurement(Uint8Array.from(vectors.hr_no_rr.bytes))
check('hr no-rr flags still parse', noRr.beatsPerMinute, vectors.hr_no_rr.bpm)
check('hr no-rr has no intervals', noRr.rrIntervalsSeconds.length, 0)
check('hr body location', hr.parseBodySensorLocation(Uint8Array.from(vectors.body_location)), 1)

// 4. PMD feature read (real-strap bytes: ECG + ACC).
const features = pmd.parsePmdFeatures(Uint8Array.from(vectors.pmd_features))
check('pmd features ecg', features.ecg, true)
check('pmd features acc', features.acc, true)
check('pmd features raw length', vectors.pmd_features.length, 15)

// 5. PMD control-point responses (get-settings success, start success, forced error).
for (const vector of vectors.pmd_responses) {
  const parsed = pmd.parseControlPointMessage(Uint8Array.from(vector.bytes))
  if (parsed.kind !== 'response') fail(`pmd response kind: got ${parsed.kind}`)
  check(`pmd op ${vector.op}`, parsed.opCode, vector.op)
  check(`pmd status ${vector.op}`, parsed.status, vector.status)
  check(`pmd statusName ${vector.op}`, parsed.statusName, vector.status_name)
  if (vector.settings) {
    const settings = pmd.parsePmdSettings(parsed.parameters)
    check('pmd settings sample rate', settings.SAMPLE_RATE, [130])
    check('pmd settings resolution', settings.RESOLUTION, [14])
  }
  console.log(`ok: pmd response op=${vector.op} status=${vector.status_name}`)
}

// 6. ECG frames: header, timestamp and exact µV samples round-trip.
for (const vector of vectors.ecg_frames) {
  const parsed = pmd.parseEcgFrame(Uint8Array.from(vector.bytes))
  check('ecg frame type', parsed.frameType, 0)
  check('ecg compressed', parsed.compressed, false)
  if (parsed.timestampNs !== BigInt(vector.timestamp_ns)) {
    fail(`ecg timestamp: got ${parsed.timestampNs}, expected ${vector.timestamp_ns}`)
  }
  console.log(`ok: ecg timestamp ${vector.timestamp_ns}`)
  check('ecg samples', [...parsed.samplesMicroVolts], vector.samples_uv)
}

console.log(`\nxcheck PASSED (${vectors.hr_measurements.length} HR, ${vectors.pmd_responses.length} PMD responses, ${vectors.ecg_frames.length} ECG frames)`)
