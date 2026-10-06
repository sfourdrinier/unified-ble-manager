#!/usr/bin/env node
'use strict'

const fs = require('node:fs')
const path = require('node:path')
const YAML = require('yaml')
const { isDeepStrictEqual } = require('node:util')

// These steps exercise live publication and cannot run in an unprivileged draft.
// Source/tag admission remains the production publisher's responsibility.
const PUBLICATION_ONLY_STEPS = [
  'Ensure npm supports trusted publishing',
  'Classify and guard canonical package release channel',
  'Check whether versions are already on npm',
  'Fetch main for initial tag verification',
  'Verify release tag points at current main',
  'Publish unified-ble-manager (OIDC + provenance)',
  'Bind npm tarball to the generated release artifact',
  'Bind npm provenance to this exact tag commit',
  'Create GitHub Release'
]

function buildDraft(production) {
  const original = structuredClone(production)
  const publishSteps = original.jobs.publish.steps
  function step(name) {
    const found = publishSteps.find(item => item.name === name)
    if (!found) throw new Error(`Production publisher has no step ${name}`)
    return structuredClone(found)
  }
  function range(first, last) {
    const start = publishSteps.findIndex(item => item.name === first)
    const end = publishSteps.findIndex(item => item.name === last)
    if (start < 0 || end < start) throw new Error(`Invalid production step range ${first}..${last}`)
    return structuredClone(publishSteps.slice(start, end + 1))
  }
  const cache = {
    name: 'Optional dependency-download cache (never shipping binaries)',
    if: '${{ !inputs.cold_cache }}',
    uses: 'actions/cache@v6',
    with: {
      path: '~/.cargo/registry\n~/.cargo/git\n~/.gradle/caches/modules-2\n~/.gradle/wrapper\n~/.npm\n~/.local/share/pnpm/store\n~/Library/pnpm/store',
      key: "draft-downloads-${{ github.job }}-${{ runner.os }}-${{ runner.arch }}-${{ hashFiles('pnpm-lock.yaml', 'Cargo.lock', 'rust-toolchain.toml', '**/gradle-wrapper.properties', '**/build.gradle') }}"
    }
  }
  const setup = () => [
    step('Checkout tag'),
    structuredClone(cache),
    {
      name: 'Setup JS package',
      uses: './.github/actions/setup-js-package',
      with: { prepack: 'false', 'node-version': '24' }
    }
  ]
  const draft = {
    name: 'Parallel publisher draft (NO PUBLICATION)',
    on: {
      workflow_dispatch: {
        inputs: {
          cold_cache: {
            description: 'Disable dependency caches for the baseline comparison',
            type: 'boolean',
            default: true
          }
        }
      }
    },
    permissions: { contents: 'read' },
    concurrency: { group: 'parallel-publisher-draft-${{ github.ref }}', 'cancel-in-progress': false },
    defaults: { run: { shell: 'bash' } },
    jobs: {}
  }
  for (const id of ['native-prebuild-plan', 'native-prebuild', 'native-rustcore']) {
    const job = structuredClone(original.jobs[id])
    if (id !== 'native-prebuild-plan') {
      const producerCache = structuredClone(cache)
      if (id === 'native-prebuild') producerCache.with.key += '-${{ matrix.rustTarget }}'
      job.steps.splice(1, 0, producerCache)
    }
    draft.jobs[id] = job
  }
  const sharedEnv = structuredClone(original.jobs.publish.env)
  function job(name, steps, needs) {
    const result = { name, 'runs-on': 'ubuntu-latest', 'timeout-minutes': 90, env: { ...sharedEnv }, steps }
    if (needs) result.needs = needs
    return result
  }
  const androidSetup = () => [
    ...setup(),
    step('Build package artifacts'),
    ...range('Setup Java (classic RN + Expo CNG Android assemble)', 'Install pinned Rust + Android targets')
  ]
  draft.jobs['android-classic'] = job('Source classic RN Android (parallel)', [
    ...androidSetup(),
    ...range('Install classic example dependencies', 'Inspect classic RN ARM32 native APK graph')
  ])
  draft.jobs['android-expo'] = job('Source Expo Android (parallel)', [
    ...androidSetup(),
    ...range('Install Expo example dependencies', 'Inspect Expo ARM32 native APK graph')
  ])
  draft.jobs['canonical-package'] = job(
    'Assemble and seal ONE candidate tarball',
    [
      ...range('Checkout tag', 'Warm cargo cache for offline metadata').filter(
        item => !PUBLICATION_ONLY_STEPS.includes(item.name)
      ),
      step('Build package artifacts'),
      step('Generate the exact canonical npm publish tarball'),
      step('Desktop-core prebuild packaging proof (maintained targets + sidecars)'),
      {
        name: 'Seal candidate source and digest',
        id: 'seal',
        run: 'cp "$PUBLISH_TARBALL" .release-package/canonical.tgz\nsha256sum .release-package/canonical.tgz | cut -d " " -f 1 | while read -r digest; do echo "sha256=$digest" >> "$GITHUB_OUTPUT"; done\nprintf "%s\\n" "$GITHUB_SHA" > .release-package/source-commit.txt'
      },
      {
        name: 'Upload immutable candidate',
        uses: 'actions/upload-artifact@v7',
        with: {
          name: 'parallel-draft-candidate',
          path: '.release-package/canonical.tgz\n.release-package/source-commit.txt',
          'if-no-files-found': 'error',
          'retention-days': 7,
          'compression-level': 0
        }
      }
    ],
    ['native-prebuild', 'native-rustcore']
  )
  draft.jobs['canonical-package'].outputs = { sha256: '${{ steps.seal.outputs.sha256 }}' }
  draft.jobs['canonical-package'].steps.splice(1, 0, structuredClone(cache))

  function packedJob(name, checks) {
    const result = job(
      name,
      [
        ...setup(),
        {
          name: 'Download immutable candidate',
          uses: 'actions/download-artifact@v8',
          with: { name: 'parallel-draft-candidate', path: '.release-package' }
        },
        {
          name: 'Verify immutable candidate digest',
          run: 'test "$(cat .release-package/source-commit.txt)" = "$GITHUB_SHA"\nprintf "%s  %s\\n" "$UBM_PACKED_TARBALL_SHA256" "$UBM_PACKED_TARBALL" | sha256sum --check\necho "PUBLISH_TARBALL=.release-package/canonical.tgz" >> "$GITHUB_ENV"\n# Stage generated artifacts only; do not overwrite checked-out sources.\ntar -xzf "$UBM_PACKED_TARBALL" --strip-components=1 package/lib package/ios/RustCore package/native/desktop-core/prebuilds'
        },
        ...checks,
        {
          name: 'Record passing source and artifact receipt',
          run: 'printf "source=%s\\ntarball_sha256=%s\\njob=%s\\n" "$GITHUB_SHA" "$UBM_PACKED_TARBALL_SHA256" "$GITHUB_JOB" >> "$GITHUB_STEP_SUMMARY"'
        }
      ],
      ['canonical-package']
    )
    result.env.UBM_PACKED_TARBALL = '${{ github.workspace }}/.release-package/canonical.tgz'
    result.env.UBM_PACKED_TARBALL_SHA256 = '${{ needs.canonical-package.outputs.sha256 }}'
    return result
  }
  draft.jobs['source-gates'] = packedJob('Source regression gates (parallel)', [
    step('Install Tauri Linux system dependencies for packed consumer'),
    step('Install pinned Rust'),
    step('Warm cargo cache for offline metadata'),
    step('Build NAPI dispatch addon (R03 converged path)'),
    ...range('Run package tests', 'Lint and typecheck'),
    ...range('Build 4.0 Web Bluetooth public example', 'Canonical host export resolve (L2 packaging)')
  ])
  draft.jobs['packed-tauri'] = packedJob('Exact candidate linked Tauri consumer', [
    step('Install pinned Rust'),
    step('Warm cargo cache for offline metadata'),
    step('Install Tauri Linux system dependencies for packed consumer'),
    step('Packed external Tauri Cargo consumer proof'),
    step('Bind linked Tauri application to exact publish tarball')
  ])
  draft.jobs['packed-smoke'] = packedJob('Exact candidate install/export consumer', [
    step('Canonical pack+install export smoke')
  ])
  draft.jobs['packed-hosts'] = packedJob('Exact candidate Expo/Tauri contracts', [
    step('Packed Expo/Tauri consumer proof')
  ])
  draft.jobs['packed-g6a'] = packedJob('Exact candidate G6A consumers', [step('G6A packed consumer proof')])
  draft.jobs['packed-tv'] = packedJob('Exact candidate Expo TV ARM32 consumer', [
    ...range('Setup Java (classic RN + Expo CNG Android assemble)', 'Setup Android SDK'),
    step('Compile exact packed Expo TV ARM32 consumer')
  ])
  draft.jobs['packed-desktop'] = packedJob('Exact candidate desktop negative acceptance', [
    step('Install pinned Rust'),
    step('Install Tauri Linux system dependencies for packed consumer'),
    step('Build NAPI dispatch addon (R03 converged path)'),
    {
      name: 'Prepare offline desktop consumer cache',
      run: 'node scripts/ci/napi-clean-tarball-acceptance.js --tarball "${PUBLISH_TARBALL}" --pm pnpm --prepare-cache'
    },
    step('Clean-tarball desktop-core acceptance (linux-x64, identity + negative legs)')
  ])
  draft.jobs.results = {
    name: 'Draft gate aggregate (cannot publish)',
    'runs-on': 'ubuntu-latest',
    'timeout-minutes': 10,
    if: '${{ always() }}',
    needs: Object.keys(draft.jobs),
    steps: [
      {
        name: 'Require every lane, report failures and candidate identity',
        env: { NEEDS_JSON: '${{ toJSON(needs) }}', SOURCE_SHA: '${{ github.sha }}' },
        run: "node <<'NODE'\nconst fs = require('node:fs');\nconst needs = JSON.parse(process.env.NEEDS_JSON);\nconst rows = Object.entries(needs).map(([name, job]) => `${name}: ${job.result}`);\nconst digest = needs['canonical-package'].outputs.sha256 || '(candidate unavailable)';\nfs.appendFileSync(process.env.GITHUB_STEP_SUMMARY, `Source: ${process.env.SOURCE_SHA}\\nTarball SHA-256: ${digest}\\n\\n${rows.join('\\n')}\\n\\nDry-run only: no npm publication, release creation, current-main admission or registry/provenance verification.\\n`);\nif (Object.values(needs).some(job => job.result !== 'success')) process.exitCode = 1;\nNODE"
      }
    ]
  }
  return draft
}

function renderDraft(production) {
  return (
    '# GENERATED by scripts/ci/generate-parallel-publisher-draft.js; do not edit.\n# Production publish.yml is preserved and remains the only publisher.\n' +
    YAML.stringify(buildDraft(production), { lineWidth: 0 })
  )
}

function buildProduction(serial) {
  const workflow = buildDraft(serial)
  workflow.name = serial.name
  workflow.on = structuredClone(serial.on)
  workflow.concurrency = { ...structuredClone(serial.concurrency), 'cancel-in-progress': false }
  // Tags have no dispatch inputs: dependency downloads may be reused, never
  // shipping binaries or previously accepted gate results.
  for (const job of Object.values(workflow.jobs)) {
    for (const step of job.steps) {
      if (step.name === 'Optional dependency-download cache (never shipping binaries)') delete step.if
      // Protected-environment approval can wait longer than a week. Keep all
      // candidate inputs for the supported public-repository retention window.
      if (step.uses?.startsWith('actions/upload-artifact@')) step.with['retention-days'] = 90
    }
  }
  workflow.jobs.results.name = 'Require ALL parallel release gates'
  workflow.jobs.results.steps[0].run = workflow.jobs.results.steps[0].run.replace(
    'Dry-run only: no npm publication, release creation, current-main admission or registry/provenance verification.',
    'All prepublication gates must succeed before the protected OIDC publisher can run.'
  )
  const originalSteps = serial.jobs.publish.steps
  const step = name => {
    const found = originalSteps.find(item => item.name === name)
    if (!found) throw new Error(`Serial publisher has no step ${name}`)
    return structuredClone(found)
  }
  const packed = workflow.jobs['packed-smoke']
  workflow.jobs.publish = {
    ...structuredClone(serial.jobs.publish),
    name: 'Publish exact qualified candidate (OIDC + provenance)',
    needs: ['results', 'canonical-package'],
    if: "${{ success() && needs.results.result == 'success' }}",
    env: { ...structuredClone(serial.jobs.publish.env), ...packed.env },
    steps: [
      step('Checkout tag'),
      step('Setup Node.js'),
      step('Ensure npm supports trusted publishing'),
      step('Enable pnpm'),
      step('Install dependencies'),
      structuredClone(packed.steps.find(item => item.name === 'Download immutable candidate')),
      structuredClone(packed.steps.find(item => item.name === 'Verify immutable candidate digest')),
      {
        name: 'Name exact candidate release asset',
        run: 'cp "$UBM_PACKED_TARBALL" ".release-package/unified-ble-manager-${GITHUB_REF_NAME#v}.tgz"\necho "PUBLISH_TARBALL=.release-package/unified-ble-manager-${GITHUB_REF_NAME#v}.tgz" >> "$GITHUB_ENV"'
      },
      ...PUBLICATION_ONLY_STEPS.filter(name => name !== 'Ensure npm supports trusted publishing').map(step)
    ]
  }
  return workflow
}

// Check the whole generated graph, not merely the presence of command strings.
// This pins each original step's condition/env/options to its intended job and
// order, together with needs, aggregate failure semantics and publish authority.
// Generator-owned changes require deliberate template + regression-test updates.
function validateProduction(workflow, serial) {
  // Compare YAML data objects (rather than structuredClone realm prototypes).
  const expected = YAML.parse(YAML.stringify(buildProduction(serial), { lineWidth: 0 }))
  const actual = YAML.parse(YAML.stringify(workflow, { lineWidth: 0 }))
  if (!isDeepStrictEqual(actual, expected)) {
    throw new Error('Unsafe parallel publisher: gate semantics or dependency context differ from the canonical graph')
  }
}

function renderProduction(serial) {
  return (
    '# GENERATED by scripts/ci/generate-parallel-publisher-draft.js --production; do not edit.\n# Serial reference is preserved in .github/publish-serial-reference.yml.\n# Trusted publisher configuration: https://www.npmjs.com/package/unified-ble-manager/access\n# Every lane checks one run-scoped source/digest; only the final job has OIDC authority.\n' +
    YAML.stringify(buildProduction(serial), { lineWidth: 0 })
  )
}

if (require.main === module) {
  const root = path.resolve(__dirname, '../..')
  const production = process.argv.includes('--production')
  const output = path.join(
    root,
    production ? '.github/workflows/publish.yml' : '.github/workflows/publish-parallel-draft.yml'
  )
  const serial = YAML.parse(fs.readFileSync(path.join(root, '.github/publish-serial-reference.yml'), 'utf8'))
  const rendered = production ? renderProduction(serial) : renderDraft(serial)
  if (process.argv.includes('--check')) {
    if (!fs.existsSync(output) || fs.readFileSync(output, 'utf8') !== rendered)
      throw new Error('Parallel draft is stale; run the generator')
  } else {
    fs.writeFileSync(output, rendered)
  }
}

module.exports = {
  buildDraft,
  renderDraft,
  buildProduction,
  renderProduction,
  validateProduction,
  PUBLICATION_ONLY_STEPS
}
