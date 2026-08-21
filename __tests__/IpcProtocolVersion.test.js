describe('desktop IPC protocol version', () => {
  test('uses the v2 channel authority', () => {
    const { IPC_BLE_PROTOCOL_CHANNEL } = require('../src/ipc/protocol')

    expect(IPC_BLE_PROTOCOL_CHANNEL).toBe('unified-ble-manager:v2')
  })
})
