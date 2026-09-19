'use strict'

// scripts/generate-scan-query-digest-corpus.js
//
// Generates __tests__/fixtures/scan-query-digests.json from the TypeScript
// contract owner (src/public/scan-query.ts normalizeScanQuery +
// src/ipc/scan-planning.ts encodeIpcScanQuery, sent through the Tauri wire
// encoder). The Rust decoder in native/tauri/src/scan_plan.rs must reproduce
// the recorded canonical JSON and digest byte-identically; its corpus test
// fails closed on any divergence.
//
// The single exception to "generated, never hand-edited" review is this
// file's ENTRIES table: adding a query shape here is how new coverage is
// added. Everything else in the fixture output is computed.
//
// Usage:
//   node scripts/generate-scan-query-digest-corpus.js          # regenerate
//   node scripts/generate-scan-query-digest-corpus.js --check  # drift check

const fs = require('node:fs')
const path = require('node:path')

const ROOT = path.resolve(__dirname, '..')
const FIXTURE_PATH = path.join(ROOT, '__tests__', 'fixtures', 'scan-query-digests.json')

// --- TypeScript loading (repo has no ts-node/tsx) --------------------------------
// Compiles the relative-only scan-query source closure with the project's own
// TypeScript compiler into a temp dir and requires it from there. Plain node
// cannot resolve the pnpm-isolated Babel presets, so Babel is not used here;
// jest remains the toolchain that loads src/ in tests.
function compileClosure() {
  const os = require('node:os')
  const { execFileSync } = require('node:child_process')
  const outDir = fs.mkdtempSync(path.join(os.tmpdir(), 'scan-query-corpus-'))
  execFileSync(
    path.join(ROOT, 'node_modules', '.bin', 'tsc'),
    [
      '--module',
      'commonjs',
      '--target',
      'es2021',
      '--lib',
      'es2021',
      '--moduleResolution',
      'node',
      '--strict',
      'false',
      '--skipLibCheck',
      '--outDir',
      path.join(outDir, 'build'),
      '--rootDir',
      'src',
      'src/tauri/transport.ts',
      'src/public/scan-query.ts',
      'src/ipc/scan-planning.ts'
    ],
    { cwd: ROOT, stdio: 'pipe' }
  )
  return path.join(outDir, 'build')
}

// --- Corpus inputs ------------------------------------------------------------
// Public ScanQuery shapes. Byte patterns use {$bytes:[...]} (mirrors the PR9
// golden fixture); the generator converts them to Uint8Array before
// normalizing, exactly as application code would pass them.
const HEART_RATE_SERVICE = '0000180d-0000-1000-8000-00805f9b34fb'

const ENTRIES = [
  {
    id: 'exact-name-heart-rate',
    description: 'Physical driver query: heart-rate service + exact Polar name (every filtered Tauri scan).',
    input: { anyOf: [{ services: { any: [HEART_RATE_SERVICE] }, names: { exact: ['Polar H10 E997042F'] } }] }
  },
  {
    id: 'prefix-name-heart-rate',
    description: 'Physical driver query: heart-rate service + Polar name prefix.',
    input: { anyOf: [{ services: { any: [HEART_RATE_SERVICE] }, names: { prefixes: ['Polar H10'] } }] }
  },
  {
    id: 'services-only-all',
    description: 'Control shape already covered by the pre-existing Rust unit test (16-bit form).',
    input: { anyOf: [{ services: { all: ['180d'] } }] }
  },
  {
    id: 'services-any-multiple',
    description: 'Several services.any entries, mixed case and short forms, unsorted input.',
    input: { anyOf: [{ services: { any: ['180F', '180d', '1800'] } }] }
  },
  {
    id: 'uuid-case-and-short-forms',
    description: 'Uppercase 128-bit, 32-bit, hyphenless 128-bit and 16-bit UUIDs canonicalize identically.',
    input: {
      anyOf: [
        {
          services: {
            any: ['0000180D-0000-1000-8000-00805F9B34FB', '0000180f00001000800000805f9b34fb'],
            all: ['180a', '00002a37-0000-1000-8000-00805f9b34fb']
          }
        }
      ]
    }
  },
  {
    id: 'manufacturer-company-only',
    description: 'Company id without any byte pattern (wire carries explicit nulls).',
    input: { anyOf: [{ manufacturerData: { any: [{ companyId: 76 }] } }] }
  },
  {
    id: 'manufacturer-prefix-no-mask',
    description: 'Byte prefix without a mask (wire carries mask null).',
    input: {
      anyOf: [{ manufacturerData: { any: [{ companyId: 76, dataPrefix: { $bytes: [2, 21] } }] } }]
    }
  },
  {
    id: 'manufacturer-prefix-mask',
    description: 'Byte prefix with a mask; all/all membership.',
    input: {
      anyOf: [
        {
          manufacturerData: {
            all: [{ companyId: 6, dataPrefix: { $bytes: [1, 2, 3] }, mask: { $bytes: [255, 255, 0] } }]
          }
        }
      ]
    }
  },
  {
    id: 'service-data-patterns',
    description: 'Service-data any/all with short-form service UUIDs and byte patterns.',
    input: {
      anyOf: [
        {
          serviceData: {
            any: [{ service: '180d', dataPrefix: { $bytes: [0] } }],
            all: [{ service: HEART_RATE_SERVICE, dataPrefix: { $bytes: [1, 2] }, mask: { $bytes: [255, 0] } }]
          }
        }
      ]
    }
  },
  {
    id: 'exclude-clause',
    description: 'Positive clause plus an exclusion clause.',
    input: {
      anyOf: [{ services: { any: [HEART_RATE_SERVICE] } }],
      exclude: [{ names: { prefixes: ['Band'] } }]
    }
  },
  {
    id: 'multiple-anyof-clauses',
    description: 'Two anyOf clauses given in reverse canonical order (normalizer sorts).',
    input: {
      anyOf: [
        { names: { exact: ['Zebra'] } },
        { names: { exact: ['Apple'] } }
      ]
    }
  },
  {
    id: 'unicode-names',
    description: 'Non-ASCII exact names and prefixes: accents, CJK, emoji.',
    input: {
      anyOf: [
        {
          names: {
            exact: ['Polaire H10', '心率带 01', 'Polar H10 💓'],
            prefixes: ['Préfixe-', '💓']
          }
        }
      ]
    }
  },
  {
    id: 'name-escaping',
    description: 'Names with quotes, backslashes and control characters exercise JSON string escaping parity.',
    input: { anyOf: [{ names: { exact: ['Quoted "Name" \\ test', 'Tab\there'], prefixes: ['Back\\'] } }] }
  },
  {
    id: 'radio-addresses',
    description: 'Lowercase and dash-separated addresses canonicalize to uppercase colons.',
    input: { anyOf: [{ addresses: ['aa:bb:cc:dd:ee:ff', 'AA-BB-CC-DD-EE-00'] }] }
  },
  {
    id: 'rssi-minimum-only',
    description: 'RSSI lower bound only (wire carries maximum null).',
    input: { anyOf: [{ services: { any: [HEART_RATE_SERVICE] }, rssi: { minimum: -70 } }] }
  },
  {
    id: 'rssi-maximum-only',
    description: 'RSSI upper bound only.',
    input: { anyOf: [{ services: { any: [HEART_RATE_SERVICE] }, rssi: { maximum: -30 } }] }
  },
  {
    id: 'rssi-range',
    description: 'RSSI closed range.',
    input: { anyOf: [{ rssi: { minimum: -80, maximum: -40 } }] }
  },
  {
    id: 'connectable-flags',
    description: 'Connectable true in one clause, false in another.',
    input: { anyOf: [{ services: { any: [HEART_RATE_SERVICE] }, connectable: true }, { connectable: false }] }
  },
  {
    id: 'peer-references',
    description: 'Durable peer reference clause.',
    input: {
      anyOf: [
        {
          peers: [{ version: 1, backendId: 'unified-ble:corebluetooth', scope: 'system', opaqueId: 'peer-1' }]
        }
      ]
    }
  },
  {
    id: 'unfiltered',
    description: 'Empty public query: unfiltered scan (the one shape that worked on hardware).',
    input: {}
  },
  {
    id: 'kitchen-sink',
    description: 'Every field in one clause plus an exclusion, exercising key order and nulls.',
    input: {
      anyOf: [
        {
          services: { any: [HEART_RATE_SERVICE], all: ['180f'] },
          names: { exact: ['Polar H10 E997042F'], prefixes: ['Polar'] },
          manufacturerData: { any: [{ companyId: 76 }] },
          serviceData: { any: [{ service: '180d', dataPrefix: { $bytes: [9] } }] },
          rssi: { minimum: -80 },
          connectable: true
        }
      ],
      exclude: [{ addresses: ['11:22:33:44:55:66'] }]
    }
  }
]

function toPublicInput(value) {
  if (Array.isArray(value)) return value.map(toPublicInput)
  if (value !== null && typeof value === 'object') {
    if (typeof value.$bytes !== 'undefined') return new Uint8Array(value.$bytes)
    return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, toPublicInput(item)]))
  }
  return value
}

function generate() {
  const build = compileClosure()
  const { normalizeScanQuery } = require(path.join(build, 'public', 'scan-query.js'))
  const { canonicalScanQueryJson } = require(path.join(build, 'backend-contract', 'scan-query.js'))
  const { encodeIpcScanQuery } = require(path.join(build, 'ipc', 'scan-planning.js'))
  const { encodeTauriWireValue } = require(path.join(build, 'tauri', 'transport.js'))

  const entries = ENTRIES.map(entry => {
    const normalized = normalizeScanQuery(toPublicInput(entry.input))
    const canonical = canonicalScanQueryJson({ anyOf: normalized.anyOf, exclude: normalized.exclude })
    if (normalized.digest !== digestOf(canonical)) {
      throw new Error(`digest mismatch while generating ${entry.id}`)
    }
    return {
      id: entry.id,
      description: entry.description,
      input: entry.input,
      canonical,
      digest: normalized.digest,
      wire: encodeTauriWireValue(encodeIpcScanQuery(normalized))
    }
  })
  return {
    generatedBy: 'scripts/generate-scan-query-digest-corpus.js from src/public/scan-query.ts + src/tauri/transport.ts',
    format: 1,
    entries
  }
}

// Local FNV-1a/64 over UTF-16 code units; must stay identical to scanQueryDigest.
function digestOf(canonical) {
  let hash = 0xcbf29ce484222325n
  for (let index = 0; index < canonical.length; index += 1) {
    hash ^= BigInt(canonical.charCodeAt(index))
    hash = BigInt.asUintN(64, hash * 0x100000001b3n)
  }
  return `scan-query-v1:${hash.toString(16).padStart(16, '0')}`
}

function main() {
  const check = process.argv.includes('--check')
  const corpus = generate()
  const serialized = `${JSON.stringify(corpus, null, 2)}\n`
  if (check) {
    const current = fs.readFileSync(FIXTURE_PATH, 'utf8')
    if (current !== serialized) {
      console.error(`scan-query digest corpus is stale: run node scripts/generate-scan-query-digest-corpus.js`)
      process.exit(1)
    }
    console.log(`scan-query digest corpus is current (${corpus.entries.length} entries)`)
    return
  }
  fs.mkdirSync(path.dirname(FIXTURE_PATH), { recursive: true })
  fs.writeFileSync(FIXTURE_PATH, serialized)
  console.log(`wrote ${FIXTURE_PATH} (${corpus.entries.length} entries)`)
}

main()
