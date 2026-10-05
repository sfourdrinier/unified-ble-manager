const fs = require('node:fs')
const path = require('node:path')
const YAML = require('yaml')
const {
  buildProduction,
  renderProduction,
  PUBLICATION_ONLY_STEPS
} = require('../scripts/ci/generate-parallel-publisher-draft')
const root = path.resolve(__dirname, '..')
const baselineText = fs.readFileSync(path.join(root, '.github/publish-serial-reference.yml'), 'utf8')
const baseline = YAML.parse(baselineText)

test('parallel production retains tag-only trust and gates publication on every lane', () => {
  const workflow = buildProduction(baseline)
  expect(workflow.on).toEqual(baseline.on)
  expect(workflow.permissions).toEqual({ contents: 'read' })
  expect(workflow.jobs.publish.environment).toBe('npm')
  expect(workflow.jobs.publish.permissions).toEqual(baseline.jobs.publish.permissions)
  expect(workflow.jobs.publish.needs).toEqual(['results', 'canonical-package'])
  expect(workflow.jobs.publish.if).toContain("needs.results.result == 'success'")
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
    const matches = steps.filter(step => step.name === original.name)
    expect(matches.length).toBeGreaterThan(0)
    expect(matches.some(step => step.run === original.run && step.uses === original.uses)).toBe(true)
  }
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

test('production is generated from preserved serial reference and does not alter it', () => {
  expect(fs.readFileSync(path.join(root, '.github/workflows/publish.yml'), 'utf8')).toBe(renderProduction(baseline))
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
