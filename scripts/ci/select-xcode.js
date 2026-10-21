// scripts/ci/select-xcode.js

'use strict'

const fs = require('node:fs')
const path = require('node:path')
const { execFileSync } = require('node:child_process')

function parseXcodeVersion(output) {
  const match = /^Xcode (\d+)\.(\d+)(?:\.(\d+))?$/m.exec(output)
  if (match === null) throw new Error(`Cannot parse Xcode version: ${output}`)
  return match.slice(1).map(component => Number(component ?? 0))
}

function assertSeries(series) {
  if (series !== '26' && series !== '27') throw new Error('Xcode series must be 26 or 27')
}

function selectXcode(installations, series) {
  assertSeries(series)
  const minimumMinor = series === '26' ? 4 : 0
  const candidates = installations
    .map(installation => ({ ...installation, components: parseXcodeVersion(installation.version) }))
    .filter(candidate => candidate.components[0] === Number(series) && candidate.components[1] >= minimumMinor)
    .sort((left, right) => right.components[1] - left.components[1] || right.components[2] - left.components[2])
  if (candidates.length === 0) throw new Error(`No installed Xcode ${series}.${minimumMinor}+ in series ${series}`)
  return candidates[0]
}

function main(argv) {
  if (argv.length !== 2 || argv[0] !== '--series') throw new Error('Usage: select-xcode.js --series 26|27')
  const series = argv[1]
  assertSeries(series)
  const installations = []
  for (const entry of fs.readdirSync('/Applications', { withFileTypes: true })) {
    // Versioned stable installations and the ordinary local Xcode.app only.
    if (!/^Xcode(?:_\d+(?:\.\d+)*)?\.app$/.test(entry.name)) continue
    const applicationPath = path.join('/Applications', entry.name)
    const version = execFileSync('/usr/bin/xcodebuild', ['-version'], {
      encoding: 'utf8',
      env: { ...process.env, DEVELOPER_DIR: path.join(applicationPath, 'Contents/Developer') }
    })
    installations.push({ path: applicationPath, version })
  }
  const selected = selectXcode(installations, series)
  execFileSync('sudo', ['xcode-select', '-s', selected.path], { stdio: 'inherit' })
  console.log(`Selected ${selected.path}\n${selected.version.trim()}`)
}

if (require.main === module) {
  try {
    main(process.argv.slice(2))
  } catch (error) {
    console.error(`select-xcode: ${error instanceof Error ? error.message : String(error)}`)
    process.exitCode = 1
  }
}

module.exports = { parseXcodeVersion, selectXcode }
