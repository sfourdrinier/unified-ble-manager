// __tests__/ParallelPublisherProduction.test.js

const fs = require('node:fs')
const path = require('node:path')
const os = require('node:os')
const { spawnSync } = require('node:child_process')
const YAML = require('yaml')
const {
  buildProduction,
  renderProduction,
  validateProduction,
  PUBLICATION_ONLY_STEPS
} = require('../scripts/ci/generate-parallel-publisher-draft')
const root = path.resolve(__dirname, '..')
const baselineText = fs.readFileSync(path.join(root, '.github/publish-serial-reference.yml'), 'utf8')
const baseline = YAML.parse(baselineText)
// The production publisher runs on Ubuntu. Register its POSIX shell harness
// only on POSIX hosts; every host still validates the complete workflow graph.

function runGeneratedBind({ readyAfter, tarballReady = true, attempts }) {
  const workflow = buildProduction(baseline)
  const bind = workflow.jobs.publish.steps.find(
    step => step.name === 'Bind npm tarball to the generated release artifact'
  )
  const temporary = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-publisher-bind-test-'))
  const bin = path.join(temporary, 'bin')
  fs.mkdirSync(bin)
  fs.writeFileSync(path.join(temporary, 'package.json'), JSON.stringify({ version: '5.1.1' }))
  fs.writeFileSync(path.join(temporary, 'SBOM.cdx.json'), 'sbom')
  fs.writeFileSync(path.join(temporary, 'THIRD_PARTY_LICENSES.json'), 'licenses')
  fs.writeFileSync(path.join(temporary, 'candidate.tgz'), 'candidate tarball')
  fs.writeFileSync(path.join(temporary, 'registry.tgz'), 'candidate tarball')
  fs.writeFileSync(
    path.join(temporary, 'attestations.json'),
    JSON.stringify({
      url: 'https://registry.npmjs.org/unified-ble-manager/attestations',
      provenance: { predicateType: 'https://slsa.dev/provenance/v1' }
    })
  )
  fs.writeFileSync(
    path.join(bin, 'npm'),
    `#!/bin/sh
set -eu
count_file="${temporary}/npm-count"
count=0
if [ -f "$count_file" ]; then count=$(cat "$count_file"); fi
count=$((count + 1))
printf '%s' "$count" > "$count_file"
case "$*" in
  *'dist.tarball'*)
    if [ "$count" -le ${readyAfter} ] || [ "${tarballReady}" != true ]; then exit 1; fi
    printf '%s\\n' '"https://registry.npmjs.org/unified-ble-manager/-/unified-ble-manager-5.1.1.tgz"'
    ;;
  *'dist.attestations'*)
    cat "${temporary}/attestations.json"
    ;;
  *) exit 1 ;;
esac
`
  )
  fs.writeFileSync(
    path.join(bin, 'curl'),
    `#!/bin/sh
set -eu
output=''
previous=''
for arg in "$@"; do
  if [ "$previous" = '--output' ]; then output="$arg"; fi
  previous="$arg"
done
case "$*" in
  *attestations*) cp "${temporary}/attestations.json" "$output" ;;
  *) cp "${temporary}/registry.tgz" "$output" ;;
esac
`
  )
  fs.writeFileSync(path.join(bin, 'sleep'), '#!/bin/sh\nexit 0\n')
  for (const name of ['npm', 'curl', 'sleep']) fs.chmodSync(path.join(bin, name), 0o755)
  try {
    const script = attempts
      ? bind.run.replace('TARBALL_MAX_ATTEMPTS=720', `TARBALL_MAX_ATTEMPTS=${attempts}`)
      : bind.run
    return spawnSync('bash', ['-e', '-o', 'pipefail', '-c', script], {
      cwd: temporary,
      encoding: 'utf8',
      env: {
        ...process.env,
        PATH: `${bin}:${process.env.PATH}`,
        PUBLISH_TARBALL: path.join(temporary, 'candidate.tgz'),
        ALREADY_PUBLISHED: 'false',
        GITHUB_STEP_SUMMARY: path.join(temporary, 'summary.md')
      }
    })
  } finally {
    fs.rmSync(temporary, { recursive: true, force: true })
  }
}

test('production publisher uses the Linux host exercised by the POSIX bind harness', () => {
  expect(buildProduction(baseline).jobs.publish['runs-on']).toBe('ubuntu-latest')
})

if (process.platform !== 'win32') {
  test('production bind waits beyond the old window and verifies digest, provenance, and checksums', () => {
    const run = runGeneratedBind({ readyAfter: 241 })
    expect(run.error).toBeUndefined()
    expect(run.status).toBe(0)
    expect(run.stdout).toContain('attempt 240/720')
  })

  test('production bind timeout explains accepted-upload recovery without republishing', () => {
    const run = runGeneratedBind({ readyAfter: 721, attempts: 3 })
    expect(run.error).toBeUndefined()
    expect(run.status).toBe(1)
    expect(run.stdout + run.stderr).toMatch(/upload may already be accepted/i)
    expect(run.stdout + run.stderr).toMatch(/do not republish|do not retag/i)
    expect(run.stdout + run.stderr).toMatch(/rerun.*failed job|wait.*public/i)
  })
}

test('isolated desktop acceptance prepares cache and its debug negative fixture before offline probes', () => {
  const workflow = buildProduction(baseline)
  const steps = workflow.jobs['packed-desktop'].steps
  const names = steps.map(step => step.name)
  const acceptance = names.indexOf('Clean-tarball desktop-core acceptance (linux-x64, identity + negative legs)')
  for (const name of [
    'Install pinned Rust',
    'Build NAPI dispatch addon (R03 converged path)',
    'Prepare offline desktop consumer cache'
  ]) {
    expect(names.indexOf(name)).toBeGreaterThan(-1)
    expect(names.indexOf(name)).toBeLessThan(acceptance)
  }
  expect(steps.find(step => step.name === 'Prepare offline desktop consumer cache').run).toContain('--prepare-cache')
  expect(steps[acceptance].run).toContain('--negative')
})

test('release guide describes the parallel graph without serial Tauri-before-Android promises', () => {
  const guide = fs.readFileSync(path.join(root, 'RELEASE.md'), 'utf8')
  expect(guide).not.toContain('Cargo consumer runs immediately after `prepack`, before examples, Android')
  expect(guide).toContain('Android/example lanes run independently')
  expect(guide).toContain('packed Tauri consumer depends on the sealed canonical package')
  expect(guide).toContain('every required lane succeeds before publication')
})

test('parallel production retains tag-only trust and gates publication on every lane', () => {
  const workflow = buildProduction(baseline)
  expect(workflow.on).toEqual(baseline.on)
  expect(workflow.permissions).toEqual({ contents: 'read' })
  expect(workflow.jobs.publish.environment).toBe('npm')
  expect(workflow.jobs.publish.permissions).toEqual(baseline.jobs.publish.permissions)
  expect(workflow.jobs.publish.needs).toEqual(['results', 'canonical-package'])
  expect(workflow.jobs.publish.if).toBe("${{ success() && needs.results.result == 'success' }}")
  for (const [id, job] of Object.entries(workflow.jobs)) {
    if (id === 'publish') continue
    expect(job.environment).toBeUndefined()
    expect(job.permissions).toBeUndefined()
  }
  expect(workflow.jobs.results.if).toBe('${{ always() }}')
  expect(workflow.jobs.results.steps[0].run).toContain("job.result !== 'success'")
  expect(workflow.jobs.results.needs.slice().sort()).toEqual(
    Object.keys(workflow.jobs)
      .filter(id => !['publish', 'results'].includes(id))
      .sort()
  )
})

test('every original release command and registry/provenance safeguard remains', () => {
  const workflow = buildProduction(baseline)
  const steps = Object.values(workflow.jobs).flatMap(job => job.steps)
  for (const original of baseline.jobs.publish.steps) {
    if (original.name === 'Bind npm tarball to the generated release artifact') continue
    expect(steps).toContainEqual(original)
  }
  expect(() => validateProduction(workflow, baseline)).not.toThrow()
  const publication = workflow.jobs.publish.steps
  for (const name of PUBLICATION_ONLY_STEPS) {
    if (name === 'Bind npm tarball to the generated release artifact') continue
    expect(publication).toContainEqual(baseline.jobs.publish.steps.find(step => step.name === name))
  }
  const originalBind = baseline.jobs.publish.steps.find(
    step => step.name === 'Bind npm tarball to the generated release artifact'
  )
  const generatedBind = publication.find(step => step.name === originalBind.name)
  expect(generatedBind.env).toEqual(originalBind.env)
  expect(generatedBind.run).toContain('TARBALL_MAX_ATTEMPTS=720')
  expect(generatedBind.run).toContain('do not republish or retag')
  expect(generatedBind.run).toContain('rerun this failed job while the candidate is retained')
  const restoredBind = {
    ...generatedBind,
    run: generatedBind.run
      .replace(
        '# npm may accept an upload before its metadata and tarball become public.\n# Give npm processing a bounded 60-minute window.\nTARBALL_MAX_ATTEMPTS=720',
        '# npm accepted rc.10 but took longer than ten minutes to expose its\n# 118 MB tarball. Give processing twenty minutes, still bounded.\nTARBALL_MAX_ATTEMPTS=240'
      )
      .replace(
        'echo "npm registry tarball did not become downloadable within the bounded 60-minute window"\n  echo "The upload may already be accepted: do not republish or retag. Wait until the version is public, then rerun this failed job while the candidate is retained. If the candidate expires, rerun the entire immutable-tag workflow."',
        'echo "npm registry tarball did not become downloadable within the bounded retry window"'
      )
  }
  expect(restoredBind).toEqual(originalBind)
  expect(publication.find(step => step.name === 'Publish unified-ble-manager (OIDC + provenance)').run).toContain(
    'npm publish "${PUBLISH_TARBALL}" --provenance'
  )
  const stage = publication.find(step => step.name === 'Verify immutable candidate digest')
  expect(stage.run).toContain('source-commit.txt')
  expect(stage.run).toContain('sha256sum --check')
  expect(workflow.jobs.publish.env.UBM_PACKED_TARBALL_SHA256).toBe('${{ needs.canonical-package.outputs.sha256 }}')
})

describe('parallel gate mutation rejection', () => {
  const cases = [
    [
      'disabled package test',
      workflow => {
        workflow.jobs['source-gates'].steps.find(step => step.name === 'Run package tests').if = false
      }
    ],
    [
      'nonblocking package test',
      workflow => {
        workflow.jobs['source-gates'].steps.find(step => step.name === 'Run package tests')['continue-on-error'] = true
      }
    ],
    [
      'nonblocking source lane',
      workflow => {
        workflow.jobs['source-gates']['continue-on-error'] = true
      }
    ],
    [
      'disabled source lane',
      workflow => {
        workflow.jobs['source-gates'].if = false
      }
    ],
    [
      'gate moved to unrelated lane',
      workflow => {
        const source = workflow.jobs['source-gates'].steps
        workflow.jobs['android-expo'].steps.push(
          source.splice(
            source.findIndex(step => step.name === 'Run package tests'),
            1
          )[0]
        )
      }
    ],
    [
      'duplicate command in unrelated lane cannot substitute for the gate',
      workflow => {
        const gate = workflow.jobs['source-gates'].steps.find(step => step.name === 'Run package tests')
        workflow.jobs['android-expo'].steps.push({ ...gate })
        gate.if = false
      }
    ],
    [
      'gate moved before candidate verification',
      workflow => {
        const source = workflow.jobs['source-gates'].steps
        source.unshift(
          source.splice(
            source.findIndex(step => step.name === 'Run package tests'),
            1
          )[0]
        )
      }
    ],
    [
      'candidate dependency removed',
      workflow => {
        workflow.jobs['packed-tauri'].needs = []
      }
    ],
    [
      'aggregate dependency dropped',
      workflow => {
        workflow.jobs.results.needs.pop()
      }
    ],
    [
      'nonblocking aggregate command',
      workflow => {
        workflow.jobs.results.steps[0]['continue-on-error'] = true
      }
    ],
    [
      'weakened publisher condition',
      workflow => {
        workflow.jobs.publish.if = "${{ always() || needs.results.result == 'success' }}"
      }
    ],
    [
      'publisher bypasses aggregate',
      workflow => {
        workflow.jobs.publish.needs = ['canonical-package']
      }
    ],
    [
      'publication operation reordered',
      workflow => {
        workflow.jobs.publish.steps.reverse()
      }
    ]
  ]
  test.each(cases)('%s fails closed', (_name, mutate) => {
    const workflow = buildProduction(baseline)
    mutate(workflow)
    expect(() => validateProduction(workflow, baseline)).toThrow(/parallel publisher/)
  })
})

test('production queues same-tag runs and retains every release artifact through approval holds', () => {
  const workflow = buildProduction(baseline)
  expect(workflow.concurrency['cancel-in-progress']).toBe(false)
  expect(workflow.concurrency.group).toBe(baseline.concurrency.group)
  const uploads = Object.values(workflow.jobs)
    .flatMap(job => job.steps)
    .filter(step => step.uses?.startsWith('actions/upload-artifact@'))
  expect(uploads).toHaveLength(3)
  for (const upload of uploads) expect(upload.with['retention-days']).toBe(90)
})

test('production is generated from preserved serial reference and does not alter it', () => {
  const productionText = fs.readFileSync(path.join(root, '.github/workflows/publish.yml'), 'utf8')
  expect(productionText).toBe(renderProduction(baseline))
  expect(() => validateProduction(YAML.parse(productionText), baseline)).not.toThrow()
  expect(fs.readFileSync(path.join(root, '.github/publish-serial-reference.yml'), 'utf8')).toBe(baselineText)
})

test('packed consumers use a workspace-relative publish path and release keeps its named asset', () => {
  const workflow = buildProduction(baseline)
  expect(
    workflow.jobs['packed-tv'].steps.find(step => step.name === 'Verify immutable candidate digest').run
  ).toContain('PUBLISH_TARBALL=.release-package/canonical.tgz')
  const name = workflow.jobs.publish.steps.find(step => step.name === 'Name exact candidate release asset')
  expect(name.run).toContain('cp "$UBM_PACKED_TARBALL"')
  expect(name.run).toContain('unified-ble-manager-${GITHUB_REF_NAME#v}.tgz')
  expect(name.run).not.toContain('npm pack')
})
