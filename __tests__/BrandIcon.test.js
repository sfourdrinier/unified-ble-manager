'use strict'

const fs = require('node:fs')
const path = require('node:path')

const root = path.join(__dirname, '..')
const brand = path.join(root, 'assets/brand')

function pngDimensions(name) {
  const data = fs.readFileSync(path.join(brand, name))
  expect(data.subarray(0, 8)).toEqual(Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]))
  return [data.readUInt32BE(16), data.readUInt32BE(20)]
}

test('one vector mark has reusable full-size and small raster derivatives', () => {
  const vector = fs.readFileSync(path.join(brand, 'ubm-mark.svg'), 'utf8')
  expect(vector).toContain('viewBox="0 0 128 128"')
  expect(vector).toContain('Unified BLE Manager')
  expect(pngDimensions('ubm-mark-512.png')).toEqual([512, 512])
  expect(pngDimensions('ubm-mark-32.png')).toEqual([32, 32])
})

test('project and Tauri proof consumers point at the shared mark', () => {
  const readme = fs.readFileSync(path.join(root, 'README.md'), 'utf8')
  const web = fs.readFileSync(path.join(root, 'example-web/index.html'), 'utf8')
  const tauri = JSON.parse(fs.readFileSync(path.join(root, 'example-tauri/src-tauri/tauri.conf.json'), 'utf8'))
  const packedConsumer = fs.readFileSync(path.join(root, 'scripts/ci/tauri-packed-consumer-check.js'), 'utf8')
  expect(readme).toContain('assets/brand/ubm-mark.svg')
  expect(web).toContain('../assets/brand/ubm-mark.svg')
  expect(tauri.bundle.icon).toContain('../../assets/brand/ubm-mark-512.png')
  expect(packedConsumer).toContain("path.join(root, 'assets/brand/ubm-mark-512.png')")
})
