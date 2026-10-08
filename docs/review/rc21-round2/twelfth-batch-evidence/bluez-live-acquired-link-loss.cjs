'use strict'
const assert=require('node:assert/strict')
const {execFile}=require('node:child_process')
const root=process.argv[2]
function busctl(args){return new Promise((resolve,reject)=>execFile('busctl',args,{timeout:10000},(error,stdout,stderr)=>error?reject(new Error(stderr||String(error))):resolve(stdout.trim())))}
async function dropLink(){const path='/org/bluez/hci1/dev_20_E1_5D_9E_A0_7F';const before=await busctl(['get-property','org.bluez',path,'org.bluez.Device1','Connected']);assert.equal(before,'b true');await busctl(['call','org.bluez',path,'org.bluez.Device1','Disconnect']);const after=await busctl(['get-property','org.bluez',path,'org.bluez.Device1','Connected']);assert.equal(after,'b false');return {target:path,before,after,source:'native peripheral-controller Device1.Disconnect; faithful simulator control refuses adversarial drop-link'}}
const {createBluezBleManager}=require(root+'/lib/commonjs/node-bluez.js')
async function bounded(p,ms,label){let timer;try{return await Promise.race([p,new Promise((_,reject)=>{timer=setTimeout(()=>reject(new Error(label+' deadline')),ms)})])}finally{clearTimeout(timer)}}
async function main(){
 const manager=await createBluezBleManager({adapterId:'/org/bluez/hci0',owner:'rc21-live-fd-'+process.pid})
 let scan,connection,writer,notifications
 try{
  scan=await manager.scan()
  const iterator=scan.observations[Symbol.asyncIterator]()
  let peer
  const end=Date.now()+25000
  while(Date.now()<end){const row=await bounded(iterator.next(),Math.max(1,end-Date.now()),'scan');assert.equal(row.done,false);if(row.value.kind==='terminal')throw row.value.error ?? new Error('scan terminal '+row.value.reason);if(row.value.kind==='value' && row.value.value.localName==='SIM Polar H10 0001'){peer=row.value.value.peer;break}}
  assert.ok(peer,'controlled peripheral not observed')
  await iterator.return();assert.equal((await scan.stop()).state,'released');scan=null
  connection=await manager.connect(peer,{timeoutMs:25000})
  const database=await connection.discover({timeoutMs:25000})
  const writeChar=database.characteristic('feee','fb005c51-02e7-f387-1cad-8acd2d8df0c8')
  const notifyChar=database.characteristic('180d','2a37')
  writer=await writeChar.acquireWrite({timeoutMs:15000})
  assert.ok(writer.mtuBytes>=23)
  const result=await writer.write(new Uint8Array([42]),{timeoutMs:5000})
  assert.equal(result.commitState,'unknown')
  await assert.rejects(writeChar.acquireWrite({timeoutMs:5000}),e=>e.code==='ownership.denied')
  const abort=new AbortController();abort.abort()
  await assert.rejects(writer.write(new Uint8Array([1]),{signal:abort.signal,timeoutMs:5000}),e=>e.code==='operation.aborted')
  notifications=await notifyChar.acquireNotifications({timeoutMs:15000})
  assert.ok(notifications.mtuBytes>=23)
  const values=notifications.values[Symbol.asyncIterator]()
  const event=await bounded(values.next(),10000,'native acquired notification')
  assert.equal(event.done,false);assert.equal(event.value.kind,'value')
  assert.ok(event.value.value.value instanceof Uint8Array)
  assert.ok(event.value.value.value.length>=2)
  console.log(JSON.stringify({phase:'real-public-fd',runtime:process.versions,writeMtu:writer.mtuBytes,notifyMtu:notifications.mtuBytes,writeCommit:result.commitState,notificationBytes:[...event.value.value.value],conflict:'ownership.denied',cancel:'operation.aborted',source:'native BlueZ over real USB controllers; simulated peripheral'}))
  const dropped=await dropLink()
  let terminal
  const terminalEnd=Date.now()+15000
  while(Date.now()<terminalEnd){const next=await bounded(values.next(),Math.max(1,terminalEnd-Date.now()),'acquired FD link-loss terminal');assert.equal(next.done,false,'link loss must carry a terminal');if(next.value.kind==='terminal'){terminal=next.value;break}}
  assert.ok(terminal,'acquired FD source must terminate after actual link loss')
  assert.equal(terminal.reason,'source-failed');assert.ok(terminal.error,'terminal must retain its failure')
  assert.equal((await values.next()).done,true)
  assert.equal((await writer.close()).state,'released');writer=null
  assert.equal((await notifications.close()).state,'released');notifications=null
  assert.equal((await connection.release()).state,'released');connection=null
  console.log(JSON.stringify({phase:'real-link-loss-acquired-terminal',dropped,terminal}))
  connection=await manager.connect(peer,{timeoutMs:25000})
  const fresh=await connection.discover({timeoutMs:25000})
  writer=await fresh.characteristic('feee','fb005c51-02e7-f387-1cad-8acd2d8df0c8').acquireWrite({timeoutMs:15000})
  notifications=await fresh.characteristic('180d','2a37').acquireNotifications({timeoutMs:15000})
  const freshValues=notifications.values[Symbol.asyncIterator]()
  const resumed=await bounded(freshValues.next(),10000,'fresh-generation acquired notification')
  assert.equal(resumed.done,false);assert.equal(resumed.value.kind,'value');assert.ok(resumed.value.value.value instanceof Uint8Array)
  assert.equal((await writer.close()).state,'released');writer=null
  await freshValues.return();assert.equal((await notifications.close()).state,'released');notifications=null
  assert.equal((await connection.release()).state,'released');connection=null
  console.log(JSON.stringify({phase:'fresh-generation-reacquisition',notificationBytes:[...resumed.value.value.value],source:'actual USB controllers; simulated peripheral'}))
  const counters=manager.diagnostics.resourceCounters()
  for(const [name,count] of Object.entries(counters))assert.equal(count,0,'retained public/backend resource '+name)
  console.log(JSON.stringify({phase:'zero-public-backend-counters',counters}))
 }finally{
  const cleanup=[]
  for(const [name,resource,method] of [['writer',writer,'close'],['notifications',notifications,'close'],['connection',connection,'release'],['scan',scan,'stop'],['manager',manager,'destroy']]){
   if(resource){try{const outcome=await resource[method]();cleanup.push({name,outcome});assert.equal(outcome.state,'released')}catch(error){cleanup.push({name,error:String(error)});console.error(JSON.stringify({cleanup}));throw error}}
  }
  console.log(JSON.stringify({phase:'cleanup',cleanup}))
 }
}
main().catch(error=>{console.error(error);process.exitCode=1})
