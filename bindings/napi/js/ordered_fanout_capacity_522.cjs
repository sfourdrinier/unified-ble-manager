'use strict';
// Public synthetic-addon regression: one collection must not lose a source
// occurrence merely because its initial byte budget is smaller than a peer's.
const assert = require('node:assert/strict');
const path = require('node:path');
const addon = require(process.env.UBM_NAPI_ADDON || path.join(__dirname, '..', `ubm_echo.${process.platform}-${process.arch}.node`));
const service = '0000180d-0000-1000-8000-00805f9b34fb';
const characteristic = '00002a37-0000-1000-8000-00805f9b34fb';
const selector = { serviceUuid: service, serviceOccurrence: 0, characteristicUuid: characteristic, characteristicOccurrence: 0 };
const peerId = 'peer-522';
const wait = () => new Promise(resolve => setTimeout(resolve, 5));
async function delivery(central, collectionId) {
  for (let attempt = 0; attempt < 200; attempt += 1) {
    const record = await central.pollOrderedGroup({ peerId, collectionId });
    if (JSON.parse(record.record).kind !== 'empty') return record;
    await wait();
  }
  throw new Error('timed out waiting for source occurrence');
}
async function main() {
  const central = await addon.UbmCentral.openSynthetic('fanout-capacity-522');
  try {
    await central.stageAdvertisement({ peerId, rssi: -50 });
    await central.connect({ peerId, lease: 'lease', timeoutMs: 5000 });
    const properties = { read: true, write: false, writeWithoutResponse: false, notify: true, indicate: false };
    await central.stageServices(peerId, [{ uuid: service, occurrence: 0, characteristics: [{ uuid: characteristic, occurrence: 0, properties, descriptors: [] }] }]);
    await central.discover({ peerId, lease: 'lease', timeoutMs: 5000 });
    const peer = (await central.peerRecords()).find(record => record.peerId === peerId);
    const ids = [central.reserveOrderedGroup(peerId), central.reserveOrderedGroup(peerId)];
    for (const [index, collectionId] of ids.entries()) {
      await central.openOrderedGroup({ wireVersion: 5, metadataVersion: 2, peerId, collectionId, lease: 'lease', connectionGeneration: peer.connectionGeneration, databaseGeneration: peer.databaseGeneration, members: [{ memberId: '0', selector }], maxItems: 1, maxBytes: index === 0 ? 2 : 128, maxControls: 1, timeoutMs: 5000 });
    }
    const status = JSON.parse(await central.orderedGroupStatus({ peerId, collectionId: ids[0] }));
    await central.stageNotification({ peerId, ...selector, value: Buffer.from([1, 2, 3, 4]) });
    await central.stageNotification({ peerId, ...selector, value: Buffer.from([1, 2, 3, 4]) });
    await assert.rejects(central.stageNotification({ peerId, ...selector, value: Buffer.from([9, 9, 9, 9]) }));
    for (let index = 0; index < 2; index += 1) {
      const large = await delivery(central, ids[1]);
      assert.deepEqual(JSON.parse(large.record).bytes, [1, 2, 3, 4]);
      assert.deepEqual(await central.pollOrderedGroup({ peerId, collectionId: ids[1] }), large);
      await central.acknowledgeOrderedDelivery({ peerId, collectionId: ids[1], deliveryId: large.deliveryId });
    }
    await central.increaseOrderedByteCapacity({ wireVersion: 1, peerId, collectionId: ids[0], sourceGeneration: status.sourceGeneration, maxBytes: 128 });
    const ordinals = [];
    for (let index = 0; index < 2; index += 1) {
      const recovered = await delivery(central, ids[0]);
      const value = JSON.parse(recovered.record);
      assert.equal(value.kind, 'value');
      assert.deepEqual(value.bytes, [1, 2, 3, 4]);
      ordinals.push(value.ordinal);
      assert.deepEqual(await central.pollOrderedGroup({ peerId, collectionId: ids[0] }), recovered);
      await central.acknowledgeOrderedDelivery({ peerId, collectionId: ids[0], deliveryId: recovered.deliveryId });
    }
    assert.notEqual(ordinals[0], ordinals[1]);
    for (const collectionId of ids) {
      assert.equal(JSON.parse((await central.pollOrderedGroup({ peerId, collectionId })).record).kind, 'empty');
    }
  } finally {
    await central.close();
  }
}
main().catch(error => { console.error(error); process.exitCode = 1; });
