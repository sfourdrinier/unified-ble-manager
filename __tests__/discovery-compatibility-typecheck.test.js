const { execFileSync } = require('child_process')
const path = require('path')

test('the original DiscoveryEvent union remains exhaustive and the v1 view is exported', () => {
  execFileSync(process.execPath, [require.resolve('typescript/bin/tsc'), '--noEmit', '-p', path.join(__dirname, 'fixtures/tsconfig.discovery-compat.json')], {
    cwd: path.resolve(__dirname, '..'),
    stdio: 'pipe'
  })
}, 30000)
