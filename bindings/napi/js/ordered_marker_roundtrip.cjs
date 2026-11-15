// bindings/napi/js/ordered_marker_roundtrip.cjs
'use strict';
const assert = require('node:assert/strict');
const path = require('node:path');
const addon = require(process.env.UBM_NAPI_ADDON || path.join(__dirname, '..', `ubm_echo.${process.platform}-${process.arch}.node`));
const service = '0000180d-0000-1000-8000-00805f9b34fb';
const characteristic = '00002a37-0000-1000-8000-00805f9b34fb';
const selector = {serviceUuid:service,serviceOccurrence:0,characteristicUuid:characteristic,characteristicOccurrence:0};
async function main() {
  const central = await addon.UbmCentral.openSynthetic('marker-native');
  try {
    assert.equal(central.orderedMarkerWireVersion(), 2);
    assert.equal(central.orderedMarkerWriteSupported(), true);
    await central.stageAdvertisement({peerId:'peer-1',rssi:-50});
    await central.connect({peerId:'peer-1',lease:'lease-a',timeoutMs:5000});
    const properties={read:true,write:true,writeWithoutResponse:true,notify:true,indicate:false};
    await central.stageServices('peer-1',[{uuid:service,occurrence:0,characteristics:[{uuid:characteristic,occurrence:0,properties,descriptors:[]}]}]);
    central.stageGattSnapshotIdentity('peer-1','synthetic-owner','1','1');
    await central.discover({peerId:'peer-1',lease:'lease-a',timeoutMs:5000});
    const peer=(await central.peerRecords()).find(row=>row.peerId==='peer-1');
    const collectionId=central.reserveOrderedGroup('peer-1');
    await central.openOrderedGroup({wireVersion:5,metadataVersion:2,peerId:'peer-1',collectionId,lease:'lease-a',connectionGeneration:peer.connectionGeneration,databaseGeneration:peer.databaseGeneration,members:[{memberId:'0',selector}],maxItems:8,maxBytes:1024,maxControls:4,timeoutMs:5000});
    const group={wireVersion:2,peerId:'peer-1',collectionId};
    const beforeLegacy = await central.stagedRadioCalls();
    await assert.rejects(central.reserveOrderedMarkerWrite({...group,wireVersion:1,reservationKey:'legacy'}));
    assert.deepEqual(await central.stagedRadioCalls(),beforeLegacy);
    const reservationId=await central.reserveOrderedMarkerWrite({...group,reservationKey:'synthetic-marker-1'});
    const id={...group,reservationId};
    const start={...id,selector,value:Buffer.from([42]),withResponse:true};
    await central.startOrderedMarkerWrite(start);
    let first;
    for (let n=0;n<200;n++) { first=JSON.parse(await central.orderedMarkerWriteStatus(id)); if(first.state!=='pending') break; await new Promise(resolve=>setTimeout(resolve,5)); }
    assert.equal(first.state,'admitted');
    assert.deepEqual(JSON.parse(await central.startOrderedMarkerWrite(start)),first);
    await assert.rejects(central.forgetOrderedMarkerWrite(id));
    let delivery;
    for (let n=0;n<200;n++) {
      delivery=await central.pollOrderedGroup({peerId:'peer-1',collectionId});
      if (delivery.deliveryId) break;
      await new Promise(resolve=>setTimeout(resolve,5));
    }
    assert.deepEqual(JSON.parse(delivery.record),{kind:'marker',markerId:'0',ordinal:first.ordinal});
    await central.acknowledgeOrderedDelivery({peerId:'peer-1',collectionId,deliveryId:delivery.deliveryId});
    central.stageSettleMarkerWrite(collectionId,'0');
    const current=JSON.parse(await central.orderedMarkerWriteStatus(id));
    assert.equal(current.state,'admitted');
    assert.equal(current.write.state,'settled');
    await central.forgetOrderedMarkerWrite(id);
    await central.forgetOrderedMarkerWrite(id);
    console.log('marker native admission, replay, FIFO acknowledgement and forget passed');
  } finally { await central.close(); }
}
main().catch(error=>{console.error(error);process.exitCode=1});
