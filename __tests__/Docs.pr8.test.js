const fs = require('fs')
const path = require('path')

const root = path.join(__dirname, '..')
const read = relativePath => fs.readFileSync(path.join(root, relativePath), 'utf8').replace(/\r\n/g, '\n')
const currentMigration = migration => migration.slice(0, migration.indexOf('## Historical RC1'))

describe('PR8 documentation contract', () => {
  test('documents the controls facade, truthful capabilities, bounded operations, and GATT recovery', () => {
    const migration = read('MIGRATION_4.0.md')
    const semantics = read('docs/UNIFIED_SEMANTICS.md')
    const documents = `${migration}\n${semantics}`

    expect(migration).not.toContain('connection:request-att-mtu')
    expect(migration).not.toMatch(/connection\.(readRssi|requestMtu)\b/)
    expect(migration).toContain('connection.controls.readRssi')
    expect(migration).toContain('connection.controls.requestMtu')
    expect(migration).toContain('connection.controls.maximumWriteLength')
    expect(migration).toContain("connection.rediscoverGatt({ reason: 'service-changed' })")
    expect(migration).toContain("connection.rediscoverGatt({ reason: 'manual' })")

    for (const id of [
      'connection:rssi',
      'connection:effective-mtu',
      'connection:request-mtu',
      'connection:priority',
      'connection:parameters',
      'connection:phy',
      'connection:subrate',
      'gatt:maximum-write-length',
      'gatt:write-without-response-readiness'
    ]) {
      expect(documents).toContain(`\`${id}\``)
    }

    for (const field of [
      'state',
      'connectionGeneration',
      'observedAtMonotonicMs',
      'source',
      'authority',
      'limitations'
    ]) {
      expect(semantics).toContain(`\`${field}\``)
    }
    expect(semantics).toMatch(/request(?:ed)?[^\n]*observation|observation[^\n]*request(?:ed)?/i)
    expect(semantics).toMatch(/one serialized queue per physical\s+connection/)
    expect(semantics).toContain('bounded')
    expect(semantics).toMatch(/Different connection\s+queues may proceed concurrently/)
    expect(semantics).toContain('gatt:write-without-response-readiness')
    expect(semantics).toMatch(/write-without-response\s+readiness[\s\S]{0,200}unsupported[\s\S]{0,200}advertises/i)
    expect(documents).toContain("reason: 'service-changed' | 'manual'")
    expect(documents).toContain('BluetoothGatt.refresh()')
    expect(documents).toMatch(/uncertain write/i)
    expect(documents).toMatch(/deterministic|host compile/i)
    expect(documents).toMatch(
      /not[\s\S]{0,120}physical-radio[\s\S]{0,120}qualification|physical-radio[\s\S]{0,120}not[\s\S]{0,120}qualification/i
    )
  })

  test('current application recipes use the public timeout and object GATT APIs', () => {
    const migration = read('MIGRATION_4.0.md')
    const readme = read('README.md')
    const current = `${currentMigration(migration)}\n${readme}`

    expect(migration).toContain('## Historical RC1')
    expect(migration).toMatch(/Historical RC1[\s\S]{0,120}non-copyable/i)
    expect(current).not.toMatch(/\bmanager\.(adapterState|adapterStates|monotonicNow)\s*\(/)
    expect(current).not.toMatch(/\b(?:scanUntil|resolveCharacteristicPath)\b/)
    expect(current).not.toMatch(/\bdeadline\b/)
    expect(current).not.toMatch(/\b(?:read|write|subscribe)\(path\b/)
    expect(current).not.toMatch(/delivery:\s*\{[\s\S]*?itemCapacity/)
    expect(current).not.toMatch(
      /import\s+(?:type\s+)?\{[^}]*\b(?:capacity|deadline|scanUntil|BackendContractError)\b[^}]*\}\s+from\s+['"]unified-ble-manager(?:\/backend-sdk)?['"]/s
    )
    expect(migration).not.toContain('hostSessionScope')

    for (const document of [migration, readme]) {
      expect(document).toContain('timeoutMs')
      expect(document).toContain('signal')
    }
    expect(migration).toContain('database.characteristic(')
    expect(migration).toContain("response: 'required'")
    expect(migration).toContain('chunkSize: maxWrite.maximumWriteLength')
  })

  test('README exposes controls through the generation-bound facade and runtime host truth', () => {
    const readme = read('README.md')
    const connection = readme.slice(readme.indexOf('### `Connection`'), readme.indexOf('### `GattDatabase`'))
    const gatt = readme.slice(readme.indexOf('### `GattDatabase`'), readme.indexOf('### `Subscription`'))

    expect(connection).toContain('connection.controls.readRssi')
    expect(connection).toContain('connection.controls.requestMtu')
    expect(connection).toContain('connection.controls.maximumWriteLength')
    expect(connection).toContain('connection.controls.writeReadiness')
    expect(connection).toMatch(/runtime|instantiated backend|host truth/i)
    expect(connection).toMatch(/readiness[\s\S]{0,180}unsupported/i)
    expect(connection).not.toMatch(/\|\s*`(?:readRssi|requestMtu|maximumWriteLength)\(/)

    expect(gatt).toContain('characteristic(serviceUuid, characteristicUuid')
    expect(gatt).toContain('characteristic.read(options)')
    expect(gatt).toContain('characteristic.write(value')
    expect(gatt).toContain('characteristic.writeLong(value')
    expect(gatt).toContain('response')
    expect(gatt).toContain('chunkSize')
    expect(gatt).toContain('characteristic.subscribe')
    expect(gatt).not.toMatch(/\b(?:read|write|writeLong|maximumWriteLength|subscribe)\(path\b/)
  })

  test('records current PR8 host truth and keeps absent controls explicit', () => {
    const readme = read('README.md')
    const semantics = read('docs/UNIFIED_SEMANTICS.md')

    expect(readme).not.toMatch(/\bconnection\.(?:readRssi|requestMtu)\s*\(/)
    expect(semantics).toContain('### 17.2 Current PR8 host matrix')
    expect(semantics).toMatch(/React Native Android[\s\S]{0,700}limited[\s\S]{0,700}deterministic/i)
    expect(semantics).toMatch(/effectiveMtu\(\)[\s\S]{0,220}onMtuChanged[\s\S]{0,220}unavailable before measurement/i)
    expect(semantics).toMatch(/React Native Android[\s\S]{0,900}onPhyRead/i)
    expect(semantics).toMatch(/onPhyUpdate[\s\S]{0,180}observation/i)
    expect(semantics).toMatch(/requestPhy\(\)[\s\S]{0,180}accepted/i)
    expect(semantics).toMatch(/direct CoreBluetooth Node\/Electron-main[\s\S]{0,500}canSendWriteWithoutResponse/i)
    expect(semantics).toMatch(/peripheralIsReady\(toSendWriteWithoutResponse:\)/)
    expect(semantics).toMatch(/connection:parameters[\s\S]{0,120}unsupported/i)
    expect(semantics).toMatch(/connection:subrate[\s\S]{0,120}unsupported/i)
    expect(semantics).toMatch(/writeWhenReady[\s\S]{0,220}capability\.unsupported/i)
  })

  test('root API report records the exact public writeWhenReady method contract', () => {
    const report = read('etc/api/root.api.md')

    expect(report).toContain('readonly security: BleSecurity')
    expect(report).toContain('readonly state?: BlePeerState')
    expect(report).toMatch(
      /export interface GattCharacteristic \{[\s\S]*readonly uuid: string[\s\S]*readonly occurrence: number[\s\S]*readonly service: GattService[\s\S]*readonly properties: GattCharacteristicProperties[\s\S]*readonly access: GattAccessRequirements[\s\S]*readonly descriptors: readonly GattDescriptor\[\][\s\S]*read\(options\?: OperationOptions\): Promise<Uint8Array>[\s\S]*write\(value: Uint8Array, options\?: GattWriteOptions\): Promise<GattWriteReceipt>[\s\S]*writeWhenReady\(value: Uint8Array, options\?: OperationOptions\): Promise<GattWriteReceipt>[\s\S]*writeLong\(value: Uint8Array, options\?: LongWriteOptions\): Promise<GattLongWriteReceipt>[\s\S]*subscribe\(options\?: GattSubscribeOptions\): Promise<GattSubscription>[\s\S]*withSubscription<T>\(options: GattSubscribeOptions, action: \(subscription: GattSubscription\) => Promise<T>\): Promise<T>[\s\S]*descriptor\(uuid: UuidInput, selector\?: OccurrenceSelector\): GattDescriptor[\s\S]*\}/
    )
  })
})
