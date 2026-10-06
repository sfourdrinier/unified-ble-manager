const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')
const childProcess = require('node:child_process')

test('acceptance prepares metadata online without scripts, then keeps probes offline', () => {
  const spawn = jest.spyOn(childProcess, 'spawnSync').mockImplementation((command, args, options) => {
    const installed = path.join(options.cwd, 'node_modules', 'unified-ble-manager')
    fs.mkdirSync(installed, { recursive: true })
    fs.writeFileSync(path.join(installed, 'package.json'), '{}')
    return { status: 0, stdout: '', stderr: '' }
  })
  try {
    jest.isolateModules(() => {
      const { install, parseArguments } = require('../scripts/ci/napi-clean-tarball-acceptance')
      expect(parseArguments(['--tarball', 'candidate.tgz', '--prepare-cache']).prepareCache).toBe(true)
      install('/candidate.tgz', 'pnpm', false)
      install('/candidate.tgz', 'pnpm')
    })
    expect(spawn.mock.calls[0][1]).toEqual(['add', '--ignore-scripts', '/candidate.tgz'])
    expect(spawn.mock.calls[1][1]).toEqual(['add', '--ignore-scripts', '--offline', '/candidate.tgz'])
  } finally {
    for (const [, , options] of spawn.mock.calls) {
      if (options.cwd.startsWith(path.join(os.tmpdir(), 'ubm-napi-acceptance-consumer-'))) {
        fs.rmSync(options.cwd, { recursive: true, force: true })
      }
    }
    spawn.mockRestore()
  }
})

test('install failures retain pnpm stdout diagnostics as well as stderr', () => {
  const spawn = jest.spyOn(childProcess, 'spawnSync').mockReturnValue({
    status: 1,
    stdout: 'ERR_PNPM_NO_OFFLINE_META @babel/runtime',
    stderr: 'additional stderr'
  })
  try {
    jest.isolateModules(() => {
      const { install } = require('../scripts/ci/napi-clean-tarball-acceptance')
      expect(() => install('/candidate.tgz', 'pnpm')).toThrow(
        /ERR_PNPM_NO_OFFLINE_META @babel\/runtime[\s\S]*additional stderr/
      )
    })
  } finally {
    spawn.mockRestore()
  }
})
