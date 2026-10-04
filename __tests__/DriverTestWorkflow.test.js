const fs = require('fs')
const path = require('path')
const YAML = require('yaml')

const root = path.resolve(__dirname, '..')
const read = file => fs.readFileSync(path.join(root, file), 'utf8')
const driverCommand = 'pnpm test:driver'
const referenceCommand = 'pnpm typecheck:references'
const expoReferenceCommand = 'pnpm typecheck:references:expo'

test('clean package preflight runs the pinned workspace formatting gate before package pretests', () => {
  const workflow = YAML.parse(read('.github/workflows/ci.yml'))
  expect(workflow.jobs['rust-5-0'].steps).toContainEqual(
    expect.objectContaining({ run: 'cargo fmt --all -- --check', if: "runner.os == 'Linux'" })
  )
  const preflight = read('scripts/ci/preflight.sh')
  const body = preflight.slice(preflight.indexOf('run_package()'), preflight.indexOf('run_tauri()'))
  const command = 'rustup run "$PINNED_TOOLCHAIN" cargo fmt --all -- --check'
  expect(body.split(command)).toHaveLength(2)
  expect(body.indexOf(command)).toBeLessThan(body.indexOf('pnpm test:package'))
})

test('canonical driver gate installs a fresh frozen Expo consumer snapshot before its unchanged test suite', () => {
  const scripts = JSON.parse(read('package.json')).scripts
  expect(scripts['test:driver']).toBe(
    'node examples-shared/dev/install-example-dependencies.js example-expo --force --frozen-lockfile && pnpm --dir example-expo test:driver'
  )
})

test('one canonical reference typecheck covers every root-installed host after prepack in CI and clean preflight', () => {
  const scripts = JSON.parse(read('package.json')).scripts
  const configs = ['examples-shared/driver', 'example-node', 'example-web', 'example-tauri', 'example-electron/driver']
  expect(scripts['typecheck:references'].split(' && ')).toEqual(
    configs.map(directory => `tsc --noEmit -p ${directory}/tsconfig.json`)
  )
  const steps = YAML.parse(read('.github/workflows/ci.yml')).jobs.package.steps
  const checks = steps.filter(step => step.run === referenceCommand)
  expect(checks).toHaveLength(1)
  expect(checks[0].if).toBe("runner.os == 'Linux' && matrix.node == '22'")
  expect(steps.indexOf(checks[0])).toBeGreaterThan(steps.findIndex(step => step.run === 'pnpm prepack'))
  const preflight = read('scripts/ci/preflight.sh')
  const body = preflight.slice(preflight.indexOf('run_package()'), preflight.indexOf('run_tauri()'))
  expect(body.split(referenceCommand)).toHaveLength(2)
  expect(body.indexOf(referenceCommand)).toBeGreaterThan(body.indexOf('pnpm prepack'))
  expect(body).not.toContain('example-expo install')
  for (const config of configs) expect(body).not.toContain(`${config}/tsconfig.json`)
})

test('Expo reference check stays mandatory after its separate dependency install in CI and preflight', () => {
  const scripts = JSON.parse(read('package.json')).scripts
  expect(scripts['typecheck:references:expo']).toBe('pnpm --dir example-expo exec tsc --noEmit -p tsconfig.json')
  const jobs = Object.values(YAML.parse(read('.github/workflows/ci.yml')).jobs)
  const job = jobs.find(candidate => candidate.steps?.some(step => step.run === expoReferenceCommand))
  expect(job).toBeDefined()
  const index = job.steps.findIndex(step => step.run === expoReferenceCommand)
  expect(index).toBeGreaterThan(
    job.steps.findIndex(step => step.run === 'node examples-shared/dev/install-example-dependencies.js example-expo --no-frozen-lockfile')
  )
  expect(index).toBeGreaterThan(job.steps.findIndex(step => step.run === 'node examples-shared/dev/install-example-dependencies.js --expo-fix example-expo'))
  const preflight = read('scripts/ci/preflight.sh')
  const body = preflight.slice(preflight.indexOf('run_android()'))
  expect(body.split(expoReferenceCommand)).toHaveLength(2)
  expect(body.indexOf(expoReferenceCommand)).toBeGreaterThan(body.indexOf('node examples-shared/dev/install-example-dependencies.js --expo-fix example-expo'))
  expect(preflight).toContain('skipped (--fast; includes Expo reference typecheck)')
  expect(preflight).toContain('skipped (no Android SDK or JDK; includes Expo reference typecheck)')
  expect(read('examples-shared/driver/README.md')).toContain(referenceCommand)
  expect(read('examples-shared/driver/README.md')).toContain(expoReferenceCommand)
})

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
    '../example-node/__tests__',
    '../example-tauri/__tests__',
    '../example-electron/driver/__tests__',
    'src/driver/__tests__'
  ]) {
    expect(scripts['test:driver']).toContain(`${directory}/*.test.mjs`)
    expect(packageBody).not.toContain(`${directory}/*.test.mjs`)
  }
})
