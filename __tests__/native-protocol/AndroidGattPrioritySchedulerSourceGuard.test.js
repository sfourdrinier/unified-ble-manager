const fs = require('fs')
const path = require('path')

const root = path.resolve(__dirname, '../..')

function read(relativePath) {
  return fs.readFileSync(path.join(root, relativePath), 'utf8')
}

function sliceFunction(source, signature, nextSignature) {
  const start = source.indexOf(signature)
  expect(start).toBeGreaterThanOrEqual(0)
  const end = source.indexOf(nextSignature, start)
  expect(end).toBeGreaterThan(start)
  return source.slice(start, end)
}

describe('Android GATT connection-priority scheduler boundary', () => {
  test('routes priority through the per-device queue with cancellable result ownership', () => {
    const radio = read('android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt')
    const priority = sliceFunction(
      radio,
      'fun requestConnectionPriority(',
      'internal fun destroy()'
    )

    expect(priority).toContain('onResult: (Result<Boolean>) -> Unit')
    expect(priority).toContain('): Long {')
    expect(priority).toContain('return enqueue(')
    expect(priority).toMatch(/onCancelled\s*=\s*\{[\s\S]*onResult\(Result\.failure/)
    expect(priority).toContain('onStartFailure = { error -> onResult(Result.failure(error)) }')
    expect(priority).toContain('gatt.requestConnectionPriority(connectionPriority)')
    expect(priority).toContain('done()')
    expect(priority).not.toMatch(/fun requestConnectionPriority\([^)]*\): Boolean/)
  })

  test('waits for the queued priority result before emitting the v2 accepted boolean', () => {
    const dispatcher = read(
      'android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolAndroidDispatcher.kt'
    )
    const priority = sliceFunction(dispatcher, 'private fun requestPriority(', 'private fun subscribe(')

    expect(priority).toContain('val radioOperationId = radio.requestConnectionPriority(')
    expect(priority).toContain('result.fold(')
    expect(priority).toContain('onSuccess = { accepted ->')
    expect(priority).toContain('18 to ProtocolWireValue.BooleanValue(accepted)')
    expect(priority).toContain('radioOperationIds[operationKey(command)] = radioOperationId')
    expect(priority).not.toContain('val accepted = radio.requestConnectionPriority(')
  })
})
