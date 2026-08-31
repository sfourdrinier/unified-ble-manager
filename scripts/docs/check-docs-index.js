// scripts/docs/check-docs-index.js

'use strict'

const fs = require('node:fs')
const path = require('node:path')

const root = path.resolve(__dirname, '../..')
const indexPath = path.join(root, 'docs/README.md')
const llmsPath = path.join(root, 'llms.txt')
const packageJsonPath = path.join(root, 'package.json')
const repositoryBlobPrefix = 'https://github.com/sfourdrinier/unified-ble-manager/blob/main/'

const statuses = ['Current', 'Historical', 'Generated']

// Subpaths that are intentionally absent from consumer-facing entrypoint
// listings: metadata passthroughs, the Expo plugin loader, and the advanced
// surface that README routes through docs/HELPERS.md caveats instead.
const unlistedExportSubpaths = ['.', './package.json', './app.plugin.js', './advanced']

function walkMarkdown(directory) {
  const found = []
  for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
    const absolute = path.join(directory, entry.name)
    if (entry.isDirectory()) {
      found.push(...walkMarkdown(absolute))
    } else if (entry.isFile() && entry.name.endsWith('.md')) {
      found.push(absolute)
    }
  }
  return found
}

function documentedSet() {
  const rootLevel = fs
    .readdirSync(root, { withFileTypes: true })
    .filter(entry => entry.isFile() && entry.name.endsWith('.md'))
    .map(entry => path.join(root, entry.name))
  return [...rootLevel, ...walkMarkdown(path.join(root, 'docs'))]
    .map(absolute => path.relative(root, absolute).split(path.sep).join('/'))
    .sort((left, right) => left.localeCompare(right))
}

function markdownLinkTargets(document, baseDirectory) {
  const targets = []
  const regex = /\]\(([^)#\s]+)(#[^)\s]*)?\)/g
  let match
  while ((match = regex.exec(document)) !== null) {
    const target = match[1]
    if (/^[a-z]+:\/\//.test(target)) {
      targets.push({ raw: target, resolved: null })
      continue
    }
    const resolved = path
      .relative(root, path.resolve(baseDirectory, target))
      .split(path.sep)
      .join('/')
    targets.push({ raw: target, resolved })
  }
  return targets
}

function checkIndex(errors) {
  if (!fs.existsSync(indexPath)) {
    errors.push('docs/README.md is missing: the documentation index does not exist')
    return
  }
  const index = fs.readFileSync(indexPath, 'utf8')
  const links = markdownLinkTargets(index, path.join(root, 'docs'))
  const linked = new Set(links.map(link => link.resolved).filter(Boolean))

  for (const relativePath of documentedSet()) {
    if (relativePath === 'docs/README.md') {
      continue
    }
    if (!linked.has(relativePath)) {
      errors.push(`docs/README.md does not index ${relativePath}`)
    }
  }

  for (const link of links) {
    if (link.resolved && !fs.existsSync(path.join(root, link.resolved))) {
      errors.push(`docs/README.md links to ${link.raw}, which does not exist`)
    }
  }

  for (const line of index.split('\n')) {
    if (!line.startsWith('|') || !line.includes('](')) {
      continue
    }
    const cells = line.split('|').map(cell => cell.trim())
    const status = cells[cells.length - 2]
    if (!statuses.includes(status)) {
      errors.push(`docs/README.md row lacks a valid status (${statuses.join('/')}): ${line.trim()}`)
    }
  }
}

function checkLlms(errors) {
  if (!fs.existsSync(llmsPath)) {
    errors.push('llms.txt is missing: consumer agents have no machine-readable package overview')
    return
  }
  const llms = fs.readFileSync(llmsPath, 'utf8')

  const exportsMap = JSON.parse(fs.readFileSync(packageJsonPath, 'utf8')).exports
  for (const subpath of Object.keys(exportsMap)) {
    if (unlistedExportSubpaths.includes(subpath)) {
      continue
    }
    const specifier = `unified-ble-manager/${subpath.slice(2)}`
    if (!llms.includes(specifier)) {
      errors.push(`llms.txt does not document the public entrypoint ${specifier}`)
    }
  }

  const linkRegex = /\]\((https:\/\/[^)#\s]+)(#[^)\s]*)?\)/g
  let match
  while ((match = linkRegex.exec(llms)) !== null) {
    const url = match[1]
    if (!url.startsWith(repositoryBlobPrefix)) {
      continue
    }
    const relativePath = url.slice(repositoryBlobPrefix.length)
    if (!fs.existsSync(path.join(root, relativePath))) {
      errors.push(`llms.txt links to ${url}, but ${relativePath} does not exist in the repository`)
    }
  }
}

function checkReadmeReachability(errors) {
  const readme = fs.readFileSync(path.join(root, 'README.md'), 'utf8')
  if (!readme.includes('docs/README.md')) {
    errors.push('README.md does not point readers at the docs/README.md documentation index')
  }
  if (!readme.includes('llms.txt')) {
    errors.push('README.md does not point AI agents at llms.txt')
  }
}

function checkDocsIndex() {
  const errors = []
  checkIndex(errors)
  checkLlms(errors)
  checkReadmeReachability(errors)
  return errors
}

module.exports = { checkDocsIndex }

if (require.main === module) {
  const errors = checkDocsIndex()
  if (errors.length > 0) {
    console.error(`Documentation index check failed:\n${errors.map(error => `- ${error}`).join('\n')}`)
    process.exit(1)
  }
  console.log('Documentation index, llms.txt, and README agent pointers are consistent.')
}
