// Load the real source graph in isolated Node, using the pinned TypeScript
// transform rather than extracted wrapper code or a rejection-suppressing hook.
const { readFileSync } = require('node:fs')
const { transpileModule, ModuleKind, ScriptTarget } = require('typescript')
require.extensions['.ts'] = (module, filename) => {
  const output = transpileModule(readFileSync(filename, 'utf8'), {
    fileName: filename,
    compilerOptions: { module: ModuleKind.CommonJS, target: ScriptTarget.ES2022 }
  })
  module._compile(output.outputText, filename)
}
const { createPublicSecurity } = require('../../src/public/security')
const security = createPublicSecurity(
  {
    watch: () => {
      throw new Error('A missing peer must never open a watch')
    }
  },
  { resolve: async () => null },
  { capability: () => ({ state: 'supported' }) },
  () => 100
)
async function main() {
  const iterator = security
    .watch({ version: 1, backendId: 'test', scope: 'application', opaqueId: 'missing' })
    [Symbol.asyncIterator]()
  try {
    await iterator.next()
    throw new Error('Expected missing peer rejection')
  } catch (error) {
    if (error.code !== 'peer.not-found') throw error
    process.stdout.write('caught peer.not-found\n')
  }
  await new Promise(resolve => setImmediate(resolve))
}
main().catch(error => {
  process.stderr.write(String(error))
  process.exitCode = 2
})
