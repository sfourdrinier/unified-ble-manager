// __tests__/native-protocol/AppleNativeEventIngress.test.js

const fs = require('fs')
const path = require('path')

const root = path.resolve(__dirname, '../..')

function read(relativePath) {
  return fs.readFileSync(path.join(root, relativePath), 'utf8')
}

describe('Apple Native Protocol v2 bounded JSI ingress', () => {
  test('gives both React Native bridges bounded headroom for a 24-hour notification burst', () => {
    const apple = read('ios/NativeProtocol/UnifiedBleProtocolAppleExecutionState.hpp')
    const android = read('android/src/main/jni/UnifiedBleProtocolJsiBinding.cpp')

    expect(android).toContain('kMaximumQueuedRecords = 512U')
    expect(android).toContain('kMaximumQueuedBytes = 1024U * 1024U')
    expect(apple).toContain('kMaximumPreJavaScriptRecords = 512U')
    expect(apple).toContain('kMaximumPreJavaScriptBytes = 1024U * 1024U')
    expect(apple).toContain('kMaximumJavaScriptRecords = 512U')
    expect(apple).toContain('kMaximumJavaScriptBytes = 1024U * 1024U')
  })

  test('keeps post-sink delivery bounded and releases binary records on every terminal discard path', () => {
    const state = read('ios/NativeProtocol/UnifiedBleProtocolAppleExecutionState.hpp')
    const execution = read('ios/NativeProtocol/UnifiedBleProtocolAppleExecution.mm')
    const binaryDelivery = read('ios/NativeProtocol/UnifiedBleProtocolAppleBinaryDelivery.mm')
    const ordinalAllocator = read('ios/NativeProtocol/UnifiedBleProtocolAppleIngress.hpp')
    const ordinalHarness = read('ios/NativeProtocol/UnifiedBleProtocolAppleIngressTests.cpp')

    expect(state).toContain('recordsAwaitingJavaScript')
    expect(state).toContain('binaryReferencesAwaitingJavaScript')
    expect(state).toContain('bool drainScheduled = false;')
    expect(execution).toContain('scheduleJavaScriptEventDrain')
    expect(execution).toContain('scheduledGeneration')
    expect(execution).toContain('state->attachmentGeneration != scheduledGeneration')
    expect(execution).toContain('releaseQueuedBinaryReferences')
    expect(execution).toContain('javaScriptEventBufferOverflow')
    expect(execution).toContain('reserveNativeIngressOrdinal(state, true)')
    expect(ordinalAllocator).toContain('std::scoped_lock lock(stateMutex)')
    expect(ordinalHarness).toContain('std::thread')
    expect(ordinalHarness).toContain('attachmentGeneration')
    expect(binaryDelivery).toContain('binaryReferencesFromEncodedRecord')
    expect(binaryDelivery).toContain('releaseBinaryReferences')
  })

  test('extracts rejected current-record ownership and uses the attachment cleanup ledger on every discard path', () => {
    const execution = read('ios/NativeProtocol/UnifiedBleProtocolAppleExecution.mm')
    const binaryDelivery = read('ios/NativeProtocol/UnifiedBleProtocolAppleBinaryDelivery.mm')
    const state = read('ios/NativeProtocol/UnifiedBleProtocolAppleExecutionState.hpp')

    const boundedEnqueue = execution.slice(
      execution.indexOf('bool enqueueBoundedRecord'),
      execution.indexOf('protocol::ProtocolRecord javaScriptEventBufferOverflow')
    )
    expect(boundedEnqueue).toContain('rejectedBinaryReferences')
    expect(boundedEnqueue).toContain('binaryReferencesFromEncodedRecord(bytes)')
    expect(boundedEnqueue).toContain('releaseAndLedgerBinaryReferences(state, rejectedBinaryReferences')
    expect(boundedEnqueue).toContain('references.pop_back()')
    expect(binaryDelivery).toContain('appendAppleBinaryReference')
    expect(binaryDelivery).toContain('ownerToken')
    expect(state).toContain('binaryCleanupLedger')
    expect(execution).toContain('retryBinaryCleanupLedger')
    expect(execution).toContain('Apple attachment detach binary cleanup retry')
    expect(execution).toContain('Apple execution close binary cleanup retry')
  })

  test('settles a terminal only after its JavaScript sink call, fatal-terminalizing scheduling or delivery failures', () => {
    const execution = read('ios/NativeProtocol/UnifiedBleProtocolAppleExecution.mm')
    const state = read('ios/NativeProtocol/UnifiedBleProtocolAppleExecutionState.hpp')
    const ingressHarness = read('ios/NativeProtocol/UnifiedBleProtocolAppleIngressTests.cpp')
    const executionHarness = read('native/protocol/tests/AppleNativeProtocolExecutionHarness.mm')
    const resultDelivery = execution.slice(
      execution.indexOf('NativeResultDeliveryStatus deliverResult'),
      execution.indexOf('bool deliverEvent')
    )
    const drain = execution.slice(
      execution.indexOf('bool scheduleJavaScriptEventDrain'),
      execution.indexOf('protocol::ProtocolRecord failureResult')
    )

    expect(state).toContain('terminalResultsAwaitingJavaScript')
    expect(resultDelivery).toContain('result,')
    expect(drain).toContain('terminalResults.size()')
    expect(drain).toContain('state->runtime->settleResult(*terminalResults[index])')
    expect(drain.indexOf('deliverEncodedRecordToJavaScript(')).toBeLessThan(
      drain.indexOf('state->runtime->settleResult(*terminalResults[index])')
    )
    expect(drain).toContain('failAttachmentAfterTerminalAdmissionFailure')
    expect(executionHarness).toContain('UnifiedBleProtocolAppleExecution.mm')
    expect(executionHarness).toContain('class ControllableInvoker final')
    expect(executionHarness).toContain('std::barrier startTerminals(3)')
    expect(executionHarness).toContain('success(state, first)')
    expect(executionHarness).toContain('success(state, second)')
    expect(executionHarness).toContain('concurrent terminals scheduled more than one Apple drain')
    expect(executionHarness).toContain('actual scheduling-unavailable seam did not fatally close the attachment')
    expect(executionHarness).toContain('actual JSI sink failure did not fatally close the attachment')
    expect(ingressHarness).not.toContain('AppleTerminalSettlementGate')
  })

  test('executes and checks the correlated Apple bonded-peer unsupported terminal', () => {
    const executionHarness = read('native/protocol/tests/AppleNativeProtocolExecutionHarness.mm')
    expect(executionHarness).toContain('enumerateBondedPeersCommand')
    expect(executionHarness).toContain('harnessField(3U, std::string("enumerateBondedPeers"))')
    expect(executionHarness).toContain('enumerateBondedPeersCommand(1U)')
    expect(executionHarness).toContain('apple-enumerate-bonded-operation-1')
    expect(executionHarness).toContain('dispatchCommand(enumerateState, enumerateCommand)')
    expect(executionHarness).toContain('enumerateUnsupported')
    expect(executionHarness).toContain('enumerateCaused')
    expect(executionHarness).toContain('enumerateCorrelated')
    expect(executionHarness).toContain('Apple enumerateBondedPeers terminal did not settle the operation')
  })
})
