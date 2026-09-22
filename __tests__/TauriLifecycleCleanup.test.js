'use strict'

const fs = require('fs')
const path = require('path')

const root = path.resolve(__dirname, '..')
const read = relativePath => fs.readFileSync(path.join(root, relativePath), 'utf8').replace(/\r\n/g, '\n')

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

  test('binds late async resource commits to the lease that admitted them', () => {
    const dispatcher = read('native/tauri/src/btleplug_dispatcher.rs')

    expect(dispatcher).toContain('__expectedLeaseId')
    expect(dispatcher).toContain('__expectedLeaseGeneration')
    expect(dispatcher).toContain('tauri.connect-stale-lease')
    expect(dispatcher).toContain('tauri.scan-stale-lease')
    expect(dispatcher).toContain('tauri.subscribe-stale-lease')
    // PR210-08 contract update (justified): core work admitted for a caller
    // that can no longer own it is compensated by its own core identity —
    // the exact link lease, the exact scan operation id — never by a blind
    // global stop that could hit a replacement caller's scan, and a failed
    // compensation is retained as orphan debt that the next release retries
    // and reports (behavior proven in native/tauri/src/dispatcher_packet_b_tests.rs).
    expect(dispatcher).toContain('OrphanResource::Link { peer_id, lease }')
    expect(dispatcher).toContain('OrphanResource::Scan(scan_id)')
    expect(dispatcher).toContain('.stop_scan(scan_id, OpControl::unbounded())')
    expect(dispatcher).toContain('orphan_debt')
    expect(dispatcher).not.toContain('authority.stop_scan().await')
    expect(dispatcher).not.toMatch(/let _ = authority\.(disconnect|unsubscribe|stop_scan)/)
    expect(dispatcher).toContain('bootstrap_admission')
    // PR210-11: lifecycle transitions are delivered by the stream handle.
    expect(dispatcher).toContain('fn emit_connection_transition')
  })
})
