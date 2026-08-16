'use strict'

const fs = require('fs')
const path = require('path')

const root = path.resolve(__dirname, '..')
const read = relativePath =>
  fs.readFileSync(path.join(root, relativePath), 'utf8').replace(/\r\n/g, '\n')

describe('Tauri caller lifecycle cleanup', () => {
  test('revokes authoritative caller ownership on navigation and window destruction', () => {
    const plugin = read('native/tauri/src/lib.rs')

    expect(plugin).toContain('fn release_caller(&self, caller: AuthenticatedCaller)')
    expect(plugin).toContain('.on_page_load(')
    expect(plugin).toContain('PageLoadEvent::Started')
    expect(plugin).toContain('.on_event(')
    expect(plugin).toContain('RunEvent::WindowEvent')
    expect(plugin).toContain('WindowEvent::Destroyed')
    expect(plugin.match(/release_caller\(/g)).toHaveLength(3)
    expect(plugin).toContain('replacement document cannot race cleanup')
  })
})
