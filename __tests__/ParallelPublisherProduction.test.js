const fs = require('node:fs')
const path = require('node:path')
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
  for (const original of baseline.jobs.publish.steps) expect(steps).toContainEqual(original)
  expect(() => validateProduction(workflow, baseline)).not.toThrow()
  const publication = workflow.jobs.publish.steps
  for (const name of PUBLICATION_ONLY_STEPS) {
    expect(publication).toContainEqual(baseline.jobs.publish.steps.find(step => step.name === name))
  }
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
