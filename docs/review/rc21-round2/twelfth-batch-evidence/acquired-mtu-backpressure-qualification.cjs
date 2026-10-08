'use strict'
// Controlled native NAPI qualification; no physical-radio claim.
const assert=require('node:assert/strict')
const root=process.argv[2]
const {bindDesktopCore}=require(root+'/lib/commonjs/desktop-core-addon.js')
const {createBluezBleManager}=require(root+'/lib/commonjs/node-bluez.js')
const addonPath=process.env.UBM_NAPI_ADDON
const bound=bindDesktopCore({platform:'bluez',operationPrefix:'bluez'},{module:require(addonPath),path:addonPath,mode:'source',sidecar:null})
let stage
const binding={diagnostics:bound.diagnostics,capabilityStates:bound.capabilityStates,openSynthetic:bound.openSynthetic,async listAdapters(){return [{index:0,label:'synthetic-adapter',error:null,displayName:null,default:true}]},async openProduction(options){stage=await bound.openSynthetic(options.owner,{platform:'bluez'});return stage}}
async function bounded(p,ms,label){let timer;try{return await Promise.race([p,new Promise((_,reject)=>timer=setTimeout(()=>reject(new Error(label+' deadline')),ms))])}finally{clearTimeout(timer)}}
async function main(){
 const manager=await createBluezBleManager({binding,owner:'rc21-acquired-mtu-'+process.pid})
 let scan,connection,writer,notifications
 try{
  scan=await manager.scan();const observed=scan.observations[Symbol.asyncIterator]();const next=observed.next()
  await stage.stageAdvertisement({peerId:'mtu-peer',localName:'Controlled FD peer'})
  const item=await bounded(next,5000,'native peer');assert.equal(item.value.kind,'value');const peer=item.value.value.peer
  await observed.return();assert.equal((await scan.stop()).state,'released');scan=null
  const services=[{uuid:'0000180d-0000-1000-8000-00805f9b34fb',occurrence:0,characteristics:[{uuid:'00002a37-0000-1000-8000-00805f9b34fb',occurrence:0,properties:{read:true,write:true,writeWithoutResponse:true,notify:true,indicate:false},descriptors:[]}]}]
  for(const mtu of [23,64]){
   connection=await manager.connect(peer)
   await stage.stageServices('mtu-peer',services)
   await stage.stageAcquiredGatt('mtu-peer',{write:true,notify:true,mtu})
   const characteristic=(await connection.discover()).characteristic('180d','2a37')
   writer=await characteristic.acquireWrite();notifications=await characteristic.acquireNotifications()
   assert.equal(writer.mtuBytes,mtu);assert.equal(notifications.mtuBytes,mtu)
   await assert.rejects(writer.write(new Uint8Array(mtu-2)),e=>e.code==='bytes.too-large')
   await stage.stageAcquiredBackpressure(true)
   const abort=new AbortController();const bytes=new Uint8Array([42]);const pending=writer.write(bytes,{signal:abort.signal,timeoutMs:5000});bytes[0]=99
   await new Promise(resolve=>setTimeout(resolve,30));abort.abort()
   await assert.rejects(pending,e=>e.code==='operation.aborted')
   assert.equal((await stage.stagedAcquiredWriteValues()).length,0)
   const closedWrite=writer.write(new Uint8Array([1]),{timeoutMs:5000});await new Promise(resolve=>setTimeout(resolve,30))
   const expectedClosed=assert.rejects(closedWrite,e=>e.code==='stream.closed' && e.operation==='bluez.gatt.acquired-write')
   assert.equal((await writer.close()).state,'released');writer=null;await expectedClosed
   assert.equal((await notifications.close()).state,'released');notifications=null
   await stage.stageAcquiredBackpressure(false)
   assert.equal((await connection.release()).state,'released');connection=null
   assert.equal(await stage.stagedAcquiredGattCount(),0)
   console.log(JSON.stringify({phase:'controlled-native-acquired-mtu-and-held-write-retirement',mtu,cancel:'operation.aborted',nativeWriteCount:0,runtime:process.versions}))
  }
  const counters=manager.diagnostics.resourceCounters();for(const [name,count] of Object.entries(counters))assert.equal(count,0,name)
  console.log(JSON.stringify({phase:'zero-public-backend-counters',counters,evidence:'controlled synthetic native radio through actual default public factory/provider/NAPI'}))
 }finally{for(const [resource,method] of [[writer,'close'],[notifications,'close'],[connection,'release'],[scan,'stop'],[manager,'destroy']])if(resource)assert.equal((await resource[method]()).state,'released')}
}
main().catch(error=>{console.error(error);process.exitCode=1})
