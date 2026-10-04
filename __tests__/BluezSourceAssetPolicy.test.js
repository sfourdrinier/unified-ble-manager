const fs = require('node:fs')
const path = require('node:path')
const os = require('node:os')
const { readBluezSourceAsset } = require('../scripts/release/bluez-source-asset')

function fixture(change, check) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-bluez-source-policy-'))
  try {
    const source = path.join(__dirname, '../vendor/bluez')
    const manifest = JSON.parse(fs.readFileSync(path.join(source, 'source-asset-manifest.json'), 'utf8'))
    for (const file of [manifest.patch.file, ...manifest.licenseFiles.map(record => record.file)]) {
      fs.copyFileSync(path.join(source, file), path.join(root, file))
    }
    change(manifest)
    fs.writeFileSync(path.join(root, 'source-asset-manifest.json'), JSON.stringify(manifest))
    check(root)
  } finally {
    fs.rmSync(root, { recursive: true, force: true })
  }
}

test.each([undefined, '5.87-ubm.0', '5.87-ubm.01', '5.88-ubm.2', '5.87-ubm.2\n'])(
  'a ready deployment requires the exact supported release grammar (%s)',
  release => {
    fixture(
      manifest => {
        manifest.distribution.linuxAuthorityContract = [1, 2, 1]
        manifest.distribution.release = release
      },
      root => expect(() => readBluezSourceAsset(root)).toThrow(/deployment.*identity/)
    )
  }
)

test('a ready deployment retains one authoritative manifest release', () => {
  fixture(
    manifest => {
      manifest.distribution.linuxAuthorityContract = [1, 2, 1]
      manifest.distribution.release = '5.87-ubm.2'
    },
    root => expect(readBluezSourceAsset(root).distribution.release).toBe('5.87-ubm.2')
  )
})

test.each([{ contract: [1, 1, 1] }, { contract: [1, 2, 2] }])(
  'obsolete or unknown deployment authority versions are refused ($contract)',
  ({ contract }) => {
    fixture(
      manifest => {
        manifest.distribution.linuxAuthorityContract = contract
        manifest.distribution.release = '5.87-ubm.2'
      },
      root => expect(() => readBluezSourceAsset(root)).toThrow(/deployment.*identity/)
    )
  }
)
