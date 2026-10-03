'use strict'
const fs = require('node:fs')
const path = require('node:path')
const { spawnSync } = require('node:child_process')

function npmCli(environment = process.env) {
  const pathKey = Object.keys(environment).find(key => key.toLowerCase() === 'path')
  for (const directory of (environment[pathKey] || '').split(path.delimiter)) {
    const executable = path.join(directory, process.platform === 'win32' ? 'npm.cmd' : 'npm')
    if (!fs.existsSync(executable)) continue
    const cli =
      process.platform === 'win32'
        ? path.join(directory, 'node_modules', 'npm', 'bin', 'npm-cli.js')
        : fs.realpathSync(executable)
    if (fs.existsSync(cli)) return cli
  }
  throw new Error('Cannot locate npm CLI on PATH for direct Node execution')
}

function runNpmPack(cwd, env, cli = npmCli(env), spawn = spawnSync) {
  return spawn(process.execPath, [cli, 'pack', '--ignore-scripts', '--json', '--loglevel=warn'], {
    cwd,
    env,
    encoding: 'utf8',
    shell: false,
    timeout: 30000
  })
}

function withCleanup(action, cleanup) {
  let primary
  let failed = false
  let result
  try {
    result = action()
  } catch (error) {
    primary = error
    failed = true
  }
  try {
    cleanup()
  } catch (error) {
    if (failed)
      throw new AggregateError([primary, error], 'Pack fixture operation and cleanup failed', { cause: primary })
    throw error
  }
  if (failed) throw primary
  return result
}

module.exports = { npmCli, runNpmPack, withCleanup }
