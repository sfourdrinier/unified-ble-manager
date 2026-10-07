'use strict'
const assert=require('node:assert/strict')
const fs=require('node:fs')
async function main(){
 const receipt=JSON.parse(fs.readFileSync('foreign-ready.json','utf8').replace(/^\uFEFF/,''))
 assert.equal(receipt.connected,'Connected');assert.equal(receipt.maintainConnection,true)
 const {createWinRtBleManager}=require('unified-ble-manager/node/winrt')
 const manager=await createWinRtBleManager({owner:'rc21-independent-inventory-'+process.pid})
 try{
  const rows=await manager.peers.connected({timeoutMs:15000})
  const peer=rows.find(row=>row.reference?.opaqueId==='public:DC:56:7B:D9:E8:A4')
  assert.ok(peer,'fresh manager must retrieve the foreign-owned native connection without scan or connect')
  assert.equal(peer.state.connection,'connected')
  const known=await manager.peers.known({timeoutMs:15000})
  const resolved=await manager.peers.resolve(peer.reference,{timeoutMs:15000})
  assert.ok(resolved,'native directory reference must round-trip without library connection admission')
  assert.deepEqual(resolved.reference,peer.reference)
  const after=await manager.peers.connected({timeoutMs:15000})
  assert.ok(after.some(p=>p.reference?.opaqueId===peer.reference.opaqueId),'foreign-owned connection must survive directory reads')
  const counters=manager.diagnostics.resourceCounters()
  for(const [name,count] of Object.entries(counters))assert.equal(count,0,name)
  console.log(JSON.stringify({phase:'independent-native-connected-inventory',runtime:process.versions,foreignOwner:receipt,peer,known:known.map(p=>({name:p.name,reference:p.reference})),resolved:resolved.reference,counters}))
 }finally{assert.equal((await manager.destroy()).state,'released')}
}
main().catch(error=>{console.error(error);process.exitCode=1})
