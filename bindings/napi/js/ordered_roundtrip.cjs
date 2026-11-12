// bindings/napi/js/ordered_roundtrip.cjs
'use strict';
const assert = require('node:assert/strict');
const path = require('node:path');
const addon = require(process.env.UBM_NAPI_ADDON || path.join(__dirname, '..', `ubm_echo.${process.platform}-${process.arch}.node`));
const service = '0000180d-0000-1000-8000-00805f9b34fb';
const characteristic = '00002a37-0000-1000-8000-00805f9b34fb';
const selector = occurrence => ({ serviceUuid: service, serviceOccurrence: 0, characteristicUuid: characteristic, characteristicOccurrence: occurrence });
const options = (collectionId) => ({ peerId: 'peer-1', collectionId, timeoutMs: 5000 });
async function take(central, collectionId, verifyReplay = false) {
  const envelope = await central.pollOrderedGroup(options(collectionId));
  if (envelope.deliveryId != null) {
    const ack = { peerId: 'peer-1', collectionId, deliveryId: envelope.deliveryId };
    if (verifyReplay) assert.deepEqual(await central.pollOrderedGroup(options(collectionId)), envelope);
    await central.acknowledgeOrderedDelivery(ack);
    if (verifyReplay) await central.acknowledgeOrderedDelivery(ack);
  }
  return JSON.parse(envelope.record);
}

let stage = 'open';
async function main() {
  const central = await addon.UbmCentral.openSynthetic('ordered-js-roundtrip');
  try {
    assert.equal(central.orderedGroupWireVersion(), 5);
    assert.equal(central.orderedMetadataVersion(), 2);
    assert.equal(central.orderedRecordingReclaimWireVersion(), 2);
    await assert.rejects(central.orderedRecordingReclaim('next', {wireVersion:1, recordingId:'old', capability:'old', lease:'old', callerKey:'old'}), /protocol\.incompatible/);
    await central.stageAdvertisement({ peerId: 'peer-1', rssi: -50 });
    await central.connect({ peerId: 'peer-1', lease: 'lease-a', timeoutMs: 5000 });
    const properties = { read: true, write: false, writeWithoutResponse: false, notify: true, indicate: false };
    await central.stageServices('peer-1', [{ uuid: service, occurrence: 0, characteristics: [0, 1].map(occurrence => ({ uuid: characteristic, occurrence, properties, descriptors: [] })) }]);
    await central.discover({ peerId: 'peer-1', lease: 'lease-a', timeoutMs: 5000 });
    await central.subscribe({ peerId: 'peer-1', lease: 'lease-a', selector: selector(0), consumer: 'existing', timeoutMs: 5000 });
    const peer = (await central.peerRecords()).find(record => record.peerId === 'peer-1');
    assert.ok(peer.connectionGeneration && peer.databaseGeneration);
    const collectionId = central.reserveOrderedGroup('peer-1');
    const open = { wireVersion: 5, metadataVersion: 2, peerId: 'peer-1', collectionId, lease: 'lease-a', connectionGeneration: peer.connectionGeneration, databaseGeneration: peer.databaseGeneration, members: [0, 1].map(n => ({ memberId: String(n), selector: selector(n) })), maxItems: 16, maxBytes: 16384, maxControls: 4, timeoutMs: 5000 };
    const callsBeforeVersionCheck = await central.stagedRadioCalls();
    const resourcesBeforeVersionCheck = await central.resourceCounters();
    for (const wireVersion of [1, 2, 3, 4, 6]) {
      await assert.rejects(central.openOrderedGroup({ ...open, wireVersion }), /protocol\.incompatible/);
    }
    for (const metadataVersion of [undefined, 1, 3]) {
      await assert.rejects(central.openOrderedGroup({ ...open, metadataVersion }), /protocol\.incompatible/);
    }
    assert.deepEqual(await central.stagedRadioCalls(), callsBeforeVersionCheck);
    assert.deepEqual(await central.resourceCounters(), resourcesBeforeVersionCheck);
    await central.openOrderedGroup(open);
    const setup = JSON.parse(await central.orderedGroupStatus(options(collectionId)));
    assert.deepEqual(setup.members.map(member => member.acquisition), ['acquired', 'acquired']);
    assert.deepEqual(setup.members.map(member => member.effectiveDelivery), ['unknown', 'unknown']);
    const capacityRequest = { wireVersion: 1, peerId: 'peer-1', collectionId, sourceGeneration: setup.sourceGeneration, maxBytes: 32768 };
    assert.equal(central.orderedCapacityWireVersion(), 1);
    await assert.rejects(central.increaseOrderedByteCapacity({ ...capacityRequest, wireVersion: 2 }), /protocol\.incompatible/);
    await assert.rejects(central.increaseOrderedByteCapacity({ ...capacityRequest, sourceGeneration: { kind: 'counter', value: '999999' } }));
    assert.deepEqual(await central.increaseOrderedByteCapacity(capacityRequest), { previousMaxBytes: 16384, maxBytes: 32768, maximumMaxBytes: 4194304 });
    assert.deepEqual(await central.increaseOrderedByteCapacity(capacityRequest), { previousMaxBytes: 32768, maxBytes: 32768, maximumMaxBytes: 4194304 });
    await assert.rejects(central.increaseOrderedByteCapacity({ ...capacityRequest, maxBytes: 16384 }));
    assert.throws(() => central.stageOrderedFailureObservation('other-peer', collectionId, 'wrong-owner'));
    for (const label of ['first', 'first', 'second']) central.stageOrderedFailureObservation('peer-1', collectionId, label);
    const observations = [];
    for (let attempt = 0; attempt < 100 && observations.length < 3; attempt += 1) {
      const record = await take(central, collectionId, true);
      if (record.kind !== 'empty') observations.push(record);
      else await new Promise(resolve => setTimeout(resolve, 5));
    }
    assert.deepEqual(observations.map(record => record.failure.platformMetadataEntries[1].value.value), ['first', 'first', 'second']);
    assert.ok(observations.every(record => record.kind === 'failure-observation'));
    assert.equal(observations[0].failure.platformMetadataEntries[0].value.value, '-9223372036854775808');
    assert.equal(observations[0].failure.platformCleanup[0].code, 'first');
    for (const [occurrence, byte] of [[0, 1], [1, 2], [0, 3]]) {
      await central.stageNotification({ peerId: 'peer-1', ...selector(occurrence), value: Buffer.from([byte]) });
    }
    central.stageOrderedSourceLoss('peer-1', '7');
    const records = [];
    const deadline = Date.now() + 5000;
    while (records.length < 4 && Date.now() < deadline) {
      const record = await take(central, collectionId, records.length === 0);
      if (record.kind !== 'empty') records.push(record);
      else await new Promise(resolve => setTimeout(resolve, 5));
    }
    assert.deepEqual(records.map(record => record.kind), ['value', 'value', 'value', 'loss']);
    assert.deepEqual(records.slice(0, 3).map(record => record.memberId), ['0', '1', '0']);
    assert.deepEqual(records.slice(0, 3).map(record => record.bytes), [[1], [2], [3]]);
    assert.equal(records[3].loss.count, '7');
    const joinedId = central.reserveOrderedGroup('peer-1');
    stage = 'join';
    await central.openOrderedGroup({ ...open, collectionId: joinedId });
    await central.stageNotification({ peerId: 'peer-1', ...selector(0), value: Buffer.from([99]) });
    let discardedDelivery;
    for (let attempt = 0; attempt < 100; attempt += 1) {
      discardedDelivery = await central.pollOrderedGroup(options(collectionId));
      if (discardedDelivery.deliveryId !== null && discardedDelivery.deliveryId !== undefined) break;
      await new Promise(resolve => setTimeout(resolve, 5));
    }
    assert.equal(JSON.parse(discardedDelivery.record).kind, 'value');
    stage = 'close driver';
    assert.equal(await central.closeOrderedGroup(options(collectionId)), 'pending-drain');
    const discardRequest = { peerId: 'peer-1', collectionId, deliveryId: discardedDelivery.deliveryId };
    const discarded = await central.discardOrderedDelivery(discardRequest);
    assert.deepEqual(await central.discardOrderedDelivery(discardRequest), discarded);
    assert.deepEqual(await central.pollOrderedDiscardReceipt(options(collectionId)), discarded);
    const evidence = JSON.parse(discarded.evidence);
    assert.equal(evidence.kind, 'value');
    assert.equal(evidence.bytes, '1');
    await central.acknowledgeOrderedDiscardReceipt({ peerId: 'peer-1', collectionId, deliveryId: discarded.receiptId });
    let joinedValue;
    for (let attempt = 0; attempt < 100; attempt += 1) {
      joinedValue = await take(central, joinedId);
      if (joinedValue.kind !== 'empty') break;
      await new Promise(resolve => setTimeout(resolve, 5));
    }
    assert.deepEqual(joinedValue.bytes, [99]);
    const terminal = await take(central, collectionId);
    assert.equal(terminal.kind, 'terminal');
    assert.equal(terminal.failureSummaryVersion, 1);
    assert.equal(terminal.failureObservationsOmitted, false);
    const driverPending = await central.closeOrderedGroup(options(collectionId));
    assert.equal(driverPending, 'pending-source');
    stage = 'close joiner';
    assert.equal(await central.closeOrderedGroup(options(joinedId)), 'pending-drain');
    assert.equal((await take(central, joinedId)).kind, 'terminal');
    let joinedOutcome = await central.closeOrderedGroup(options(joinedId));
    if (joinedOutcome === 'pending-source') {
      await new Promise(resolve => setTimeout(resolve, 20));
      joinedOutcome = await central.closeOrderedGroup(options(joinedId));
    }
    if (joinedOutcome !== 'released') console.error('joiner status', await central.orderedGroupStatus(options(joinedId)), 'driver status', await central.orderedGroupStatus(options(collectionId)), 'calls', await central.stagedRadioCalls());
    assert.equal(joinedOutcome, 'released');
    assert.equal(JSON.parse(await central.orderedGroupStatus(options(joinedId))).cleanupComplete, true);
    await central.acknowledgeOrderedGroup(options(joinedId));
    await central.forgetOrderedGroup(options(joinedId));
    assert.equal(await central.unsubscribe({ peerId: 'peer-1', selector: selector(0), consumer: 'existing' }), true);
    let driverOutcome = await central.closeOrderedGroup(options(collectionId));
    if (driverOutcome === 'pending-source') {
      await new Promise(resolve => setTimeout(resolve, 20));
      driverOutcome = await central.closeOrderedGroup(options(collectionId));
    }
    if (driverOutcome !== 'released') console.error('driver status', await central.orderedGroupStatus(options(collectionId)), 'calls', await central.stagedRadioCalls());
    assert.equal(driverOutcome, 'released');
    assert.equal(JSON.parse(await central.orderedGroupStatus(options(collectionId))).cleanupComplete, true);
    await central.acknowledgeOrderedGroup(options(collectionId));
    await central.forgetOrderedGroup(options(collectionId));
    stage = 'discard failure observation';
    const failureId = central.reserveOrderedGroup('peer-1');
    await central.openOrderedGroup({ ...open, collectionId: failureId, members: [open.members[0]] });
    central.stageOrderedFailureObservation('peer-1', failureId, 'discarded');
    let failureDelivery;
    for (let attempt = 0; attempt < 100; attempt += 1) {
      failureDelivery = await central.pollOrderedGroup(options(failureId));
      if (failureDelivery.deliveryId != null) break;
      await new Promise(resolve => setTimeout(resolve, 5));
    }
    assert.equal(JSON.parse(failureDelivery.record).kind, 'failure-observation');
    assert.equal(await central.closeOrderedGroup(options(failureId)), 'pending-drain');
    const failureDiscard = { peerId: 'peer-1', collectionId: failureId, deliveryId: failureDelivery.deliveryId };
    const failureReceipt = await central.discardOrderedDelivery(failureDiscard);
    assert.deepEqual(await central.pollOrderedDiscardReceipt(options(failureId)), failureReceipt);
    assert.equal(JSON.parse(failureReceipt.evidence).failure.platformCleanup[0].code, 'discarded');
    await central.acknowledgeOrderedDiscardReceipt({ peerId: 'peer-1', collectionId: failureId, deliveryId: failureReceipt.receiptId });
    assert.equal((await take(central, failureId)).kind, 'terminal');
    let failureClose = await central.closeOrderedGroup(options(failureId));
    if (failureClose === 'pending-source') {
      await new Promise(resolve => setTimeout(resolve, 20));
      failureClose = await central.closeOrderedGroup(options(failureId));
    }
    assert.equal(failureClose, 'released');
    await central.acknowledgeOrderedGroup(options(failureId));
    await central.forgetOrderedGroup(options(failureId));
    stage = 'capacity pressure retry';
    const overflowId = central.reserveOrderedGroup('peer-1');
    await central.openOrderedGroup({ ...open, collectionId: overflowId, maxItems: 1, members: [open.members[0]] });
    for (const byte of [1, 2]) await central.stageNotification({ peerId: 'peer-1', ...selector(0), value: Buffer.from([byte]) });
    assert.deepEqual((await take(central, overflowId)).bytes, [1]);
    assert.deepEqual((await take(central, overflowId)).bytes, [2]);
    let overflowClose = await central.closeOrderedGroup(options(overflowId));
    for (let attempt = 0; attempt < 100 && overflowClose !== 'released'; attempt += 1) {
      const item = await take(central, overflowId);
      if (item.kind === 'terminal') assert.equal(item.reason.kind, 'closed');
      overflowClose = await central.closeOrderedGroup(options(overflowId));
      if (overflowClose !== 'released') await new Promise(resolve => setTimeout(resolve, 5));
    }
    assert.equal(overflowClose, 'released');
    await central.acknowledgeOrderedGroup(options(overflowId));
    await central.forgetOrderedGroup(options(overflowId));
    stage = 'failed setup receipt';
    const failedId = central.reserveOrderedGroup('peer-1');
    await central.failNextRadioOp('subscribe', 'synthetic refusal');
    await assert.rejects(central.openOrderedGroup({ ...open, collectionId: failedId, members: [open.members[0]] }));
    let failedOutcome = 'pending-source';
    let sawTerminal = false;
    for (let attempt = 0; attempt < 100 && failedOutcome !== 'released'; attempt += 1) {
      const item = await take(central, failedId);
      if (item.kind === 'terminal') sawTerminal = true;
      failedOutcome = await central.closeOrderedGroup(options(failedId));
      if (failedOutcome !== 'released') await new Promise(resolve => setTimeout(resolve, 5));
    }
    assert.equal(sawTerminal, true);
    assert.equal(failedOutcome, 'released');
    await new Promise(resolve => setTimeout(resolve, 20));
    const failedStatus = JSON.parse(await central.orderedGroupStatus(options(failedId)));
    assert.equal(failedStatus.cleanupComplete, true);
    assert.equal(failedStatus.members[0].cleanupReleased, true);
    assert.equal(failedStatus.setupError.code, 'gatt.subscribe-failed');
    await central.acknowledgeOrderedGroup(options(failedId));
    await central.acknowledgeOrderedGroup(options(failedId));
    assert.equal(JSON.parse(await central.orderedGroupStatus(options(failedId))).cleanupComplete, true);
    await central.forgetOrderedGroup(options(failedId));
    await central.forgetOrderedGroup(options(failedId));
    await assert.rejects(central.orderedGroupStatus(options(failedId)));
    console.log('ordered N-API synthetic JS round-trip passed');
  } finally {
    await central.close();
  }
}
main().catch(error => { console.error(stage, error); process.exitCode = 1; });
