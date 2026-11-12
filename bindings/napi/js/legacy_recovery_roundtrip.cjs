'use strict'

// Real source-built N-API boundary; no radio is opened. UBM_NAPI_ADDON points
// at the temporary test build, not a distributed prebuild.
const assert = require('node:assert/strict')
const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')
const { execFileSync } = require('node:child_process')
const addon = require(process.env.UBM_NAPI_ADDON || path.join(__dirname, '..', `ubm_echo.${process.platform}-${process.arch}.node`))
const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-legacy-napi-'))
const python = String.raw`
import sqlite3, sys
p, name, damaged = sys.argv[1:]
c = sqlite3.connect(p)
c.executescript('''PRAGMA page_size=4096; PRAGMA application_id=1430408522; PRAGMA user_version=2;
CREATE TABLE journal(id INTEGER PRIMARY KEY,recording_id TEXT NOT NULL,declaration TEXT NOT NULL,max_bytes INTEGER NOT NULL,max_records INTEGER NOT NULL,phase TEXT NOT NULL,next_ordinal INTEGER NOT NULL,next_token INTEGER NOT NULL,lost INTEGER NOT NULL,pending_token TEXT,pending_last INTEGER,pending_more INTEGER,last_ack_token TEXT,last_ack_receipt TEXT,retained_records INTEGER NOT NULL,retained_bytes INTEGER NOT NULL,collection_failure TEXT);
CREATE TABLE records(ordinal INTEGER PRIMARY KEY,body TEXT NOT NULL,bytes INTEGER NOT NULL);
CREATE TABLE ordered_identities(producer_id TEXT NOT NULL,collection_id TEXT NOT NULL,generation_kind TEXT NOT NULL,generation_value TEXT NOT NULL,delivery_id TEXT NOT NULL,payload TEXT NOT NULL,ordinal INTEGER NOT NULL,PRIMARY KEY(producer_id,collection_id,generation_kind,generation_value,delivery_id)) WITHOUT ROWID;''')
bodies = [b'{broken', b'\xff\x00', b'{}'] if damaged == 'yes' else [b'{"ordinal":1,"metadata":{},"record":{}}']
sizes = [len(b) + (1 if i == 2 else 0) for i, b in enumerate(bodies)]
for i, (body, size) in enumerate(zip(bodies, sizes), 1):
 c.execute('INSERT INTO records VALUES(?,CAST(? AS TEXT),?)', (i, body, size))
c.execute("INSERT INTO journal VALUES(1,?,'{}',1048576,100,'recording',4,2,0,'recording:1',?,0,NULL,NULL,?,?,NULL)", (name,len(bodies),len(bodies),sum(sizes)))
c.commit()
`
function fixture(name, damaged = false) {
  execFileSync('python3', ['-c', python, path.join(dir, `${name}.sqlite`), name, damaged ? 'yes' : 'no'])
}
const value = async promise => { const result = JSON.parse(await promise); assert.equal(result.ok, true, JSON.stringify(result)); return result.value }
async function main() {
  fixture('damaged', true)
  fixture('valid')
  fixture('missing')
  const first = new addon.ContinuationRecordingStore()
  const second = new addon.ContinuationRecordingStore()
  await value(first.configureDirectory(dir))
  await value(second.configureDirectory(dir))
  for (const [id, expected, lengthValid] of [['1', [123, 98, 114, 111, 107, 101, 110], true], ['2', [255, 0], true], ['3', [123, 125], false]]) {
    const chunk = await value(first.inspectRawRecord('damaged', 'records', id, '0', 32))
    assert.deepEqual(chunk.bytes, expected)
    assert.equal(chunk.lengthValid, lengthValid)
    assert.equal(chunk.jsonValid, id === '3')
    assert.equal(chunk.lengthFault === null, lengthValid)
  }
  assert.equal(JSON.parse(await first.legacyRecordingPrepareExact('damaged', 3, 4096)).ok, false)
  const page = await value(first.legacyRecordingPrepareExact('valid', 1, 4096))
  const serialized = JSON.stringify(page)
  const receipts = await Promise.all([value(first.legacyRecordingAcknowledgeExact('valid', serialized)), value(second.legacyRecordingAcknowledgeExact('valid', serialized))])
  assert.deepEqual(receipts[0], receipts[1])
  const retirements = await Promise.all([value(first.legacyRecordingRetireExact('valid', serialized)), value(second.legacyRecordingRetireExact('valid', serialized))])
  assert.deepEqual(retirements[0], retirements[1])
  const retired = retirements[0] // deliberately treat the first reply as lost
  await value(second.status('valid')) // migration may win the retry race
  assert.deepEqual(await value(first.legacyRecordingRetireExact('valid', serialized)), retired)
  execFileSync(process.execPath, [__filename], { env: { ...process.env, UBM_EXACT_CHILD_DIR: dir, UBM_EXACT_CHILD_PAGE: serialized } })
  const missing = await value(first.legacyRecordingPrepareExact('missing', 1, 4096))
  await value(first.legacyRecordingAcknowledgeExact('missing', JSON.stringify(missing)))
  execFileSync('python3', ['-c', "import sqlite3,sys; c=sqlite3.connect(sys.argv[1]); c.execute('DELETE FROM legacy_exact_receipt'); c.commit()", path.join(dir, 'missing.sqlite')])
  assert.equal(JSON.parse(await second.legacyRecordingRetireExact('missing', JSON.stringify(missing))).ok, false)
  assert.equal(JSON.parse(await second.status('missing')).ok, false)
  console.log('legacy recovery real N-API boundary passed')
}
async function child() {
  const store = new addon.ContinuationRecordingStore()
  await value(store.configureDirectory(process.env.UBM_EXACT_CHILD_DIR))
  assert.equal((await value(store.legacyRecordingRetireExact('valid', process.env.UBM_EXACT_CHILD_PAGE))).retired, true)
}
(process.env.UBM_EXACT_CHILD_DIR ? child() : main()).catch(error => { console.error(error); process.exitCode = 1 }).finally(() => fs.rmSync(dir, { recursive: true, force: true }))
