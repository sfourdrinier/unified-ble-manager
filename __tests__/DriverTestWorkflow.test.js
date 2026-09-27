const fs = require('fs')
const path = require('path')
const YAML = require('yaml')

const root = path.resolve(__dirname, '..')
const read = file => fs.readFileSync(path.join(root, file), 'utf8')
const driverCommand = 'pnpm --dir example-expo test:driver'

test('Linux Node22 package CI runs the canonical cross-host driver suite after building public imports', () => {
  const workflow = YAML.parse(read('.github/workflows/ci.yml'))
  const steps = workflow.jobs.package.steps
  const driver = steps.filter(step => step.run === driverCommand)
  expect(driver).toHaveLength(1)
  expect(driver[0].if).toBe("runner.os == 'Linux' && matrix.node == '22'")
  const build = steps.findIndex(step => step.run === 'pnpm prepack')
  expect(build).toBeGreaterThan(-1)
  expect(steps.indexOf(driver[0])).toBeGreaterThan(build)
})

test('clean preflight reuses the same driver command after prepack without copying test lists', () => {
  const preflight = read('scripts/ci/preflight.sh')
  const packageStart = preflight.indexOf('run_package()')
  const tauriStart = preflight.indexOf('run_tauri()')
  expect(packageStart).toBeGreaterThan(-1)
  expect(tauriStart).toBeGreaterThan(packageStart)
  const packageBody = preflight.slice(packageStart, tauriStart)
  expect(packageBody.split(driverCommand)).toHaveLength(2)
  expect(packageBody.indexOf('pnpm prepack')).toBeGreaterThan(-1)
  expect(packageBody.indexOf(driverCommand)).toBeGreaterThan(packageBody.indexOf('pnpm prepack'))
  const scripts = JSON.parse(read('example-expo/package.json')).scripts
  for (const directory of [
    '../examples-shared/driver/__tests__',
    '../examples-shared/driver/server/__tests__',
    'src/driver/__tests__'
  ]) {
    expect(scripts['test:driver']).toContain(`${directory}/*.test.mjs`)
    expect(packageBody).not.toContain(`${directory}/*.test.mjs`)
  }
})
