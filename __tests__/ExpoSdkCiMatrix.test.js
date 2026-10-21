// __tests__/ExpoSdkCiMatrix.test.js

const fs = require('node:fs')
const path = require('node:path')
const YAML = require('yaml')

const root = path.resolve(__dirname, '..')

function readWorkflow(filename) {
  return YAML.parse(fs.readFileSync(path.join(root, '.github/workflows', filename), 'utf8'))
}

function stepByName(job, name) {
  const step = job.steps.find(candidate => candidate.name === name)
  expect(step).toBeDefined()
  return step
}

function stepIndex(job, name) {
  return job.steps.findIndex(candidate => candidate.name === name)
}

const sdkValues = ['57', '58']

describe('Expo SDK CI matrix', () => {
  test.each([
    ['ci.yml', 'expo-cng-android'],
    ['publish.yml', 'android-expo']
  ])('%s %s runs both SDK profiles with fail-fast disabled', (filename, jobId) => {
    const job = readWorkflow(filename).jobs[jobId]

    expect(job.strategy).toEqual({
      'fail-fast': false,
      matrix: { 'expo-sdk': sdkValues }
    })
    expect(job.strategy['max-parallel']).toBeUndefined()
    expect(job.name).toContain('${{ matrix.expo-sdk }}')
  })

  test('reusable iOS Expo job maps each SDK to its runner and Xcode series', () => {
    const job = readWorkflow('apple-ci.yml').jobs['ios-expo']

    expect(job.strategy).toEqual({
      'fail-fast': false,
      matrix: {
        include: [
          { 'expo-sdk': '57', runner: 'macos-26', 'xcode-series': '26' },
          { 'expo-sdk': '58', runner: 'xcode-27', 'xcode-series': '27' }
        ]
      }
    })
    expect(job.strategy['max-parallel']).toBeUndefined()
    expect(job.name).toContain('${{ matrix.expo-sdk }}')
    expect(job['runs-on']).toBe('${{ matrix.runner }}')

    const selectXcode = stepByName(job, 'Select Xcode')
    expect(selectXcode.with).toEqual({ 'xcode-series': '${{ matrix.xcode-series }}' })
  })

  test.each([
    ['ci.yml', 'expo-cng-android'],
    ['publish.yml', 'android-expo'],
    ['apple-ci.yml', 'ios-expo']
  ])('%s %s selects the SDK before the first Expo install', (filename, jobId) => {
    const job = readWorkflow(filename).jobs[jobId]
    const prepare = stepByName(job, 'Select Expo SDK profile')
    stepByName(job, 'Install Expo example dependencies')

    expect(prepare.run).toBe('node scripts/ci/prepare-expo-sdk.js --sdk ${{ matrix.expo-sdk }}')
    expect(stepIndex(job, 'Select Expo SDK profile')).toBe(stepIndex(job, 'Install Expo example dependencies') - 1)
  })

  test('CI keeps the packed TV ARM32 qualification on one SDK57 leg', () => {
    const job = readWorkflow('ci.yml').jobs['expo-cng-android']
    const tv = stepByName(job, 'Compile packed Expo TV ARM32 consumer')

    expect(tv.if).toBe("matrix.expo-sdk == '57'")
    expect(job.steps.filter(step => step.name === 'Compile packed Expo TV ARM32 consumer')).toHaveLength(1)
  })

  test('publish aggregate keeps the existing android-expo matrix job dependency', () => {
    const workflow = readWorkflow('publish.yml')
    const resultsNeeds = workflow.jobs.results.needs

    expect(resultsNeeds).toContain('android-expo')
    expect(resultsNeeds).not.toEqual(expect.arrayContaining(['android-expo-57', 'android-expo-58']))
    expect(workflow.jobs['android-classic'].strategy).toBeUndefined()
    expect(workflow.jobs['source-gates'].strategy).toBeUndefined()
  })

  test('unrelated CI and Apple jobs remain independent of the Expo matrix', () => {
    const ci = readWorkflow('ci.yml')
    const apple = readWorkflow('apple-ci.yml')

    expect(ci.jobs.package.strategy).toBeDefined()
    expect(ci.jobs.package.strategy.matrix['expo-sdk']).toBeUndefined()
    expect(ci.jobs['classic-rn-android'].strategy).toBeUndefined()
    expect(apple.jobs['ios-example'].strategy).toBeUndefined()
    expect(apple.jobs['tvos-library'].strategy).toBeUndefined()
  })

  test('affected ordinary PRs invoke automatic Expo-only Apple coverage', () => {
    const ci = readWorkflow('ci.yml')
    const apple = ci.jobs.apple

    expect(apple.if).toContain("github.base_ref != '5.0.0'")
    expect(apple.if).toContain("needs.changes.outputs.apple == 'true'")
    expect(apple.with).toEqual({
      'expo-only': "${{ github.event_name == 'pull_request' && github.base_ref != '5.0.0' && !contains(github.event.pull_request.labels.*.name, 'ci:apple') }}"
    })
  })

  test('Expo-only Apple mode skips unrelated jobs while default mode keeps full coverage', () => {
    const apple = readWorkflow('apple-ci.yml')
    const input = apple.on['workflow_call'].inputs['expo-only']

    expect(input).toEqual({
      description: 'Run only the Expo CNG matrix for an affected ordinary PR',
      required: false,
      type: 'boolean',
      default: false
    })
    expect(apple.jobs['ios-example'].if).toBe('${{ !inputs.expo-only }}')
    expect(apple.jobs['tvos-library'].if).toBe('${{ !inputs.expo-only }}')
    expect(apple.jobs['ios-expo'].if).toBeUndefined()
  })
})
