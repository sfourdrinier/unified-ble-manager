// bindings/napi/js/ordered_recording_roundtrip.cjs
'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const addon = require(process.env.UBM_NAPI_ADDON || path.join(__dirname, '..', `ubm_echo.${process.platform}-${process.arch}.node`));
const service = '0000180d-0000-1000-8000-00805f9b34fb';
const characteristic = '00002a37-0000-1000-8000-00805f9b34fb';
const selector = { serviceUuid: service, serviceOccurrence: 0, characteristicUuid: characteristic, characteristicOccurrence: 0 };
async function main() {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-ordered-recording-'));
  const central = await addon.UbmCentral.openSynthetic('ordered-recording-js');
  try {
    assert.equal(addon.orderedRecordingReceiptVersion(), 2);
    assert.equal(central.orderedRecordingReceiptVersion(), 2);
    assert.equal(JSON.parse(await central.continuationConfigureRecordingDirectory(directory)).ok, true);
    await central.stageAdvertisement({peerId:'peer-1',rssi:-50});
    await central.connect({peerId:'peer-1',lease:'lease-a',timeoutMs:5000});
    const properties = {read:true,write:false,writeWithoutResponse:false,notify:true,indicate:false};
    await central.stageServices('peer-1',[{uuid:service,occurrence:0,characteristics:[{uuid:characteristic,occurrence:0,properties,descriptors:[]}]}]);
    await central.discover({peerId:'peer-1',lease:'lease-a',timeoutMs:5000});
    const peer = (await central.peerRecords()).find(row => row.peerId === 'peer-1');
    const startHandle = central.reserveOrderedRecordingStart();
    const options = {startHandle,wireVersion:5,metadataVersion:2,recordingId:'sample',recordingMaxBytes:1048576,recordingMaxRecords:1,peerId:'peer-1',lease:'lease-a',connectionGeneration:peer.connectionGeneration,databaseGeneration:peer.databaseGeneration,members:[{memberId:'0',publicMemberId:'heart',selector}],maxItems:8,maxBytes:16384,maxControls:4,timeoutMs:5000};
    const first = await central.startOrderedRecording(options);
    assert.deepEqual(JSON.parse(first).receipt.state,'active');
    assert.equal(JSON.parse(first).receipt.version, addon.orderedRecordingReceiptVersion());
    assert.equal(await central.startOrderedRecording(options),first);
    assert.equal(JSON.parse(central.orderedRecordingStartStatus(startHandle)).state,'result');
    const beforeWrongPeer=await central.stagedRadioCalls();
    await assert.rejects(central.reserveRecordingMarkerWrite({wireVersion:2,recordingId:'sample',peerId:'other-peer',lease:'lease-a',reservationKey:'wrong-peer'}),/ownership\.denied/);
    assert.deepEqual(await central.stagedRadioCalls(),beforeWrongPeer);
    central.stageOrderedFailureObservation('peer-1', JSON.parse(first).receipt.collectionId, 'recorded');
    await central.stageNotification({peerId:'peer-1',...selector,value:Buffer.from([42])});
    const store = central.continuationRecordingStore();
    assert.equal(await store.orderedReceiptVersion('sample'), 2);
    let page;
    for (let i=0;i<200;i++) {
      page = JSON.parse(await store.prepare('sample',16,4096));
      if (page.ok && page.value.records.length) break;
      await new Promise(resolve=>setTimeout(resolve,5));
    }
    assert.equal(page.value.records.length,1);
    assert.equal(page.value.records[0].metadata.version, 2);
    assert.equal(page.value.records[0].record.kind, 'failure-observation');
    assert.equal(page.value.records[0].record.failure.platformMetadataEntries[1].value.value, 'recorded');
    assert.equal(page.value.records[0].record.failure.platformCleanup[0].code, 'recorded');
    assert.deepEqual(JSON.parse(await store.prepare('sample',16,4096)).value.records, page.value.records);
    assert.equal(JSON.parse(await store.acknowledge('sample',page.value.token)).ok,true);
    let stop;
    for (let i=0;i<200;i++) {
      stop = JSON.parse(await store.stopOrdered('sample'));
      if (stop.value?.recording.state === 'finished') break;
      const tail=JSON.parse(await store.prepare('sample',16,4096));
      if (tail.ok && tail.value.token) await store.acknowledge('sample',tail.value.token);
      await new Promise(resolve=>setTimeout(resolve,5));
    }
    assert.equal(stop.value?.recording.state,'finished');
    assert.equal(stop.value.recording.version, addon.orderedRecordingReceiptVersion());
    assert.equal(await central.startOrderedRecording(options),first);
    assert.equal(JSON.parse(await store.orderedContext('sample')).value.members[0].publicMemberId,'heart');
    await central.retireOrderedRecordingStart(startHandle);
    await central.retireOrderedRecordingStart(startHandle);
    console.log('ordered recording native start, replay, drain and stop passed');
  } finally { await central.close(); fs.rmSync(directory,{recursive:true,force:true}); }
}
main().catch(error=>{console.error(error);process.exitCode=1});
