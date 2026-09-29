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

  test.each([
    [[], 'profile.codec.malformed'],
    [[50, 51], 'profile.codec.malformed'],
    [[101], 'profile.codec.invalid-value']
  ])('preserves canonical Battery Level validation for %j', async (bytes, code) => {
    const gatt = {
      characteristic: jest.fn().mockReturnValue({ read: jest.fn().mockResolvedValue(new Uint8Array(bytes)) })
    }
    await expect(readBatteryLevel(gatt)).rejects.toMatchObject({
      name: 'ProfileCodecError',
      code,
      codec: 'Battery Level'
    })
  })
})
