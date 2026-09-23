const { readBatteryLevel } = require('../example-tauri/src/battery')

describe('the Tauri battery example', () => {
  test('normalizes short UUIDs through the public lookup and performs the read', async () => {
    const read = jest.fn().mockResolvedValue(new Uint8Array([73]))
    const characteristic = jest.fn().mockReturnValue({ read })
    const options = { timeoutMs: 5_000 }

    await expect(readBatteryLevel({ characteristic }, options)).resolves.toBe(73)
    expect(characteristic).toHaveBeenCalledWith('180f', '2a19')
    expect(read).toHaveBeenCalledWith(options)
  })

  test('does not silently accept an empty Battery Level value', async () => {
    const gatt = {
      characteristic: jest.fn().mockReturnValue({ read: jest.fn().mockResolvedValue(new Uint8Array()) })
    }
    await expect(readBatteryLevel(gatt)).rejects.toThrow('Battery Level returned an empty value')
  })
})
