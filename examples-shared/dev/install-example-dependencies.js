// Independent reference consumers own their .npmrc and lockfile settings.
'use strict'

const path = require('node:path')
const { spawnSync } = require('node:child_process')

const ROOT = path.resolve(__dirname, '..', '..')

function runPnpm(args, env = process.env) {
  const command = process.platform === 'win32' ? 'pnpm.cmd' : 'pnpm'
  const result = spawnSync(command, args, {
    cwd: ROOT,
    env,
    stdio: 'inherit',
    shell: process.platform === 'win32'
  })
  if (result.error !== undefined) throw result.error
  if (result.status !== 0) throw new Error(`pnpm ${args.join(' ')} failed (exit ${String(result.status)})`)
}

function installExampleDependencies(exampleDir, args = [], env = process.env) {
  if (typeof exampleDir !== 'string' || exampleDir.length === 0) {
    throw new Error('Usage: node examples-shared/dev/install-example-dependencies.js <example> [install flags]')
  }
  // --dir alone still inherits the ancestor pnpm-workspace.yaml during install.
  // Keep the consumer's own peer policy and lock, without hard-coding either.
  runPnpm(['--dir', exampleDir, 'install', '--ignore-workspace', ...args], env)
}

function fixExampleExpoDependencies(exampleDir, env = process.env) {
  if (typeof exampleDir !== 'string' || exampleDir.length === 0) {
    throw new Error('Usage: node examples-shared/dev/install-example-dependencies.js --expo-fix <example>')
  }
  // Expo forwards arguments after -- to pnpm add/install, including its SDK
  // upgrade follow-up. Isolate that subprocess as well as the initial install.
  runPnpm(['--dir', exampleDir, 'exec', 'expo', 'install', '--fix', '--', '--ignore-workspace'], env)
}

if (require.main === module) {
  try {
    if (process.argv[2] === '--expo-fix') {
      fixExampleExpoDependencies(process.argv[3])
    } else {
      installExampleDependencies(process.argv[2], process.argv.slice(3))
    }
  } catch (error) {
    console.error(`install-example-dependencies: ${error.message}`)
    process.exitCode = 1
  }
}

module.exports = { installExampleDependencies, fixExampleExpoDependencies, runPnpm }
