'use strict'

// The reference application is copied, never forked. Only its external build
// inputs and shared-source location change; UBM itself comes from packed bytes.
const fs = require('node:fs')
const path = require('node:path')

function validatePackedTvInput(stage, tarball) {
  if (!path.isAbsolute(tarball) || !fs.statSync(tarball).isFile()) {
    throw new Error('TV_PACKAGE_TARBALL must name an existing absolute package tarball')
  }
  const destination = fs.existsSync(stage) ? fs.realpathSync(stage) : path.resolve(stage)
  if (fs.realpathSync(tarball).startsWith(`${destination}${path.sep}`)) {
    throw new Error('TV_PACKAGE_TARBALL must be outside the disposable TV stage')
  }
}

function stagePackedTv(stage, root, tarball) {
  validatePackedTvInput(stage, tarball)
  const manifestPath = path.join(stage, 'package.json')
  const manifest = JSON.parse(fs.readFileSync(manifestPath, 'utf8'))
  // The stage is outside the checkout, so Corepack cannot see the repository
  // pin. The newest pnpm otherwise rejects same-day Expo releases and ignores
  // the manifest's pnpm.overrides.
  const repository = JSON.parse(fs.readFileSync(path.join(root, 'package.json'), 'utf8'))
  if (typeof repository.packageManager !== 'string' || repository.packageManager.length === 0) {
    throw new Error('repository package.json must declare packageManager for the packed TV consumer')
  }
  manifest.packageManager = repository.packageManager
  manifest.dependencies['unified-ble-manager'] = `file:${tarball}`
  fs.writeFileSync(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`)
  const shared = path.join(stage, '.ubm-reference-shared')
  fs.cpSync(path.join(root, 'examples-shared'), shared, {
    recursive: true,
    filter: source => !['node_modules', '.DS_Store'].includes(path.basename(source))
  })
  const metroPath = path.join(stage, 'metro.config.js')
  const metro = fs.readFileSync(metroPath, 'utf8')
  const anchor = "path.resolve(projectRoot, '../examples-shared')"
  if (!metro.includes(anchor)) throw new Error('TV reference Metro shared-source anchor missing')
  fs.writeFileSync(metroPath, metro.replace(anchor, "path.resolve(projectRoot, '.ubm-reference-shared')"))
  for (const name of ['shared.ts', 'headless-continuation-job.ts', 'headless-continuation-history.ts']) {
    const sourcePath = path.join(stage, 'src/driver', name)
    const source = fs.readFileSync(sourcePath, 'utf8')
    if (!source.includes('../../../examples-shared/')) throw new Error(`TV shared import missing in ${name}`)
    fs.writeFileSync(sourcePath, source.replaceAll('../../../examples-shared/', '../../.ubm-reference-shared/'))
  }
}

module.exports = { stagePackedTv, validatePackedTvInput }
if (require.main === module) {
  if (process.argv[2] === '--validate') validatePackedTvInput(...process.argv.slice(3))
  else stagePackedTv(...process.argv.slice(2))
}
