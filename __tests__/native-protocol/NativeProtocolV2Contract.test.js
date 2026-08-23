// __tests__/native-protocol/NativeProtocolV2.test.js

// __tests__/native-protocol/NativeProtocolV2.test.js

const childProcess = require('child_process')
const fs = require('fs')
const os = require('os')
const path = require('path')
const { encodeNativeProtocolRecord } = require('../../src/native-protocol/v2-codec')

const root = path.resolve(__dirname, '../..')
const schemaPath = path.join(root, 'native/protocol/schema/native-protocol-v2.json')

function read(relativePath) {
  return fs.readFileSync(path.join(root, relativePath), 'utf8')
}

describe('Native Protocol v2 schema authority', () => {
  function priorityCommand(priority, extraFields = []) {
    const attachment = {
      kind: 'attachment',
      fields: [
        { id: 1, value: 'attachment-1' },
        { id: 2, value: 'backend-1' },
        { id: 3, value: 'generation-1' },
        { id: 4, value: 'adapter-1' },
        { id: 5, value: 'adapter-generation-1' }
      ]
    }
    const correlation = {
      kind: 'operationCorrelation',
      fields: [
        { id: 1, value: attachment },
        { id: 2, value: 1 },
        { id: 3, value: 'priority-1' }
      ]
    }
    return {
      kind: 'command',
      fields: [
        { id: 1, value: 2 },
        { id: 2, value: correlation },
        { id: 3, value: 'requestPriority' },
        { id: 16, value: priority },
        ...extraFields
      ]
    }
  }

  test('declares the Android priority command/result and accepted boolean in the v2 source schema', () => {
    const schema = JSON.parse(fs.readFileSync(schemaPath, 'utf8'))
    expect(schema.commandKinds).toContain('requestPriority')
    expect(schema.resultKinds).toContain('priority')
    expect(schema.connectionPriorities).toEqual(['lowPower', 'balanced', 'highThroughput'])
    expect(schema.records.find(record => record.name === 'command').fields).toContainEqual([
      'connectionPriority',
      'enum:connectionPriorities',
      false
    ])
    expect(schema.records.find(record => record.name === 'result').fields).toContainEqual([
      'priorityAccepted',
      'boolean',
      false
    ])
  })

  test('rejects malformed, duplicate, and unsupported priority command values at the codec boundary', () => {
    expect(() => encodeNativeProtocolRecord(priorityCommand('highThroughput'))).not.toThrow()
    expect(() => encodeNativeProtocolRecord(priorityCommand('turbo'))).toThrow('Native protocol enum value is invalid')
    expect(() => encodeNativeProtocolRecord(priorityCommand('balanced', [{ id: 16, value: 'lowPower' }]))).toThrow(
      'Native protocol record has a duplicate field'
    )
  })

  test('locks every record, enum, and field to an explicit immutable v1 ABI wire ID', () => {
    const schema = JSON.parse(fs.readFileSync(schemaPath, 'utf8'))
    const manifest = JSON.parse(fs.readFileSync(path.join(path.dirname(schemaPath), schema.abiManifest), 'utf8'))
    expect(manifest.version).toBe(schema.abiVersion)
    expect(Object.keys(manifest.recordKinds)).toEqual(expect.arrayContaining(schema.recordKinds))
    for (const [name, values] of Object.entries(schema)) {
      if (name !== 'recordKinds' && name !== 'records' && Array.isArray(values)) {
        expect(Object.keys(manifest.enums[name])).toEqual(expect.arrayContaining(values))
      }
    }
    for (const record of schema.records) {
      const fieldIds = record.fields.map(([name]) => manifest.fields[record.name][name])
      expect(fieldIds.every(id => Number.isInteger(id) && id > 0 && id <= 65535)).toBe(true)
      expect(new Set(fieldIds).size).toBe(fieldIds.length)
    }
  })

  test('has deterministic generated C++, Android, Apple, TypeScript, and control Codegen bindings', () => {
    childProcess.execFileSync(process.execPath, ['scripts/native-protocol/generate-native-protocol.js', '--check'], {
      cwd: root,
      stdio: 'pipe'
    })
    const schema = JSON.parse(fs.readFileSync(schemaPath, 'utf8'))
    const cpp = read('native/protocol/generated/NativeProtocolV2Schema.hpp')
    const kotlin = read(
      'android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/generated/NativeProtocolV2Schema.kt'
    )
    const swift = read('ios/Generated/NativeProtocolV2Schema.swift')
    const typescript = read('src/native-protocol/generated/native-protocol-v2-schema.ts')
    for (const kind of schema.recordKinds) {
      expect(cpp).toContain(`${kind} =`)
      expect(kotlin).toContain(kind.replace(/([a-z0-9])([A-Z])/g, '$1_$2').toUpperCase())
      expect(swift).toContain(`case ${kind} =`)
      expect(typescript).toContain(`'${kind}'`)
    }
    expect(cpp).not.toContain('Base64')
    expect(kotlin).not.toContain('Base64')
    expect(swift).not.toContain('Base64')
    expect(typescript).not.toContain('Base64')
  })

  test('generates RN 0.86 supported control-only Codegen shapes on Android and Apple', () => {
    const output = fs.mkdtempSync(path.join(os.tmpdir(), 'unified-ble-native-codegen-'))
    try {
      childProcess.execFileSync(
        process.execPath,
        [
          'node_modules/react-native/scripts/generate-codegen-artifacts.js',
          '-p',
          '.',
          '-t',
          'all',
          '-o',
          output,
          '-s',
          'library',
          '-f'
        ],
        { cwd: root, stdio: 'pipe' }
      )
      const generatedFiles = listFiles(output)
      const androidSpec = generatedFiles.find(file => file.endsWith('NativeUnifiedBleProtocolControlSpec.java'))
      const appleHeader = generatedFiles.find(
        file => file.endsWith('UnifiedBleProtocolSpec.h') && file.includes(`${path.sep}ios${path.sep}`)
      )
      expect(androidSpec).toBeDefined()
      expect(appleHeader).toBeDefined()
      const android = fs.readFileSync(androidSpec, 'utf8')
      const apple = fs.readFileSync(appleHeader, 'utf8')
      const androidImplementation = read(
        'android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolControlModule.java'
      )
      const appleImplementation = read('ios/UnifiedBleProtocolControl.mm')
      const appleControlStart = apple.indexOf('namespace NativeUnifiedBleProtocolControl')
      const appleControlEnd = apple.indexOf('NS_ASSUME_NONNULL_END', appleControlStart)
      const appleControl = apple.slice(appleControlStart, appleControlEnd)
      expect(android).toContain('NativeUnifiedBleProtocolControlSpec')
      expect(android).toContain('handshake')
      expect(android).toContain('bootstrapRestorationIdentity')
      expect(android).toContain('acquireBackground')
      expect(android).toContain('releaseBackground')
      expect(androidImplementation).toContain('ubm-restoration-v1')
      expect(android).toContain('cancelOperation(ReadableMap correlation')
      expect(android).toContain('closeAttachment(ReadableMap attachment')
      expect(android).not.toMatch(/Uint8Array|ArrayBuffer|Base64/)
      expect(appleControlStart).toBeGreaterThanOrEqual(0)
      expect(appleControlEnd).toBeGreaterThan(appleControlStart)
      expect(appleControl).toContain('NativeUnifiedBleProtocolControlSpec')
      expect(appleControl).toContain('handshake')
      expect(appleControl).toContain('bootstrapRestorationIdentity')
      expect(appleImplementation).toContain('acquireBackground')
      expect(appleImplementation).toContain('releaseBackground')
      expect(appleImplementation).toContain('ubm-restoration-v1')
      expect(appleControl).not.toMatch(/Uint8Array|ArrayBuffer|Base64/)
    } finally {
      fs.rmSync(output, { recursive: true, force: true })
    }
  })

  test('registers lifecycle-owned protocol control modules on Android and Apple', () => {
    const androidPackage = read('android/src/main/java/com/sfourdrinier/unifiedblemanager/BlePlxPackage.java')
    const androidControl = read(
      'android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolControlModule.java'
    )
    const appleControl = read('ios/UnifiedBleProtocolControl.mm')
    const packageJson = JSON.parse(read('package.json'))
    expect(androidPackage).toContain('UnifiedBleProtocolControlModule.NAME')
    expect(androidControl).toContain('extends NativeUnifiedBleProtocolControlSpec')
    expect(androidControl).toContain('public synchronized void invalidate()')
    expect(appleControl).toMatch(
      /@interface\s+UnifiedBleProtocolControl\s*:\s*NSObject\s*<\s*NativeUnifiedBleProtocolControlSpec\s*,\s*RCTTurboModuleWithJSIBindings\s*>/
    )
    expect(appleControl).toContain('NativeUnifiedBleProtocolControlSpecJSI')
    expect(packageJson.codegenConfig.ios.modulesProvider).toEqual(
      expect.objectContaining({
        UnifiedBleProtocolControl: 'UnifiedBleProtocolControl'
      })
    )
  })

  test('installs the single native JSI binary runtime through metadata-only control', () => {
    const control = read('src/NativeUnifiedBleProtocolControl.ts')
    const androidControl = read(
      'android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolControlModule.java'
    )
    const androidBinding = read(
      'android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolJsiBinding.java'
    )
    const nativeBinding = read('android/src/main/jni/UnifiedBleProtocolJsiBinding.cpp')
    const runtime = read('native/protocol/src/NativeProtocolControlRuntime.cpp')

    expect(control).toContain('installExecutionRuntime')
    expect(androidControl).toContain('UnifiedBleProtocolJsiBinding.install')
    expect(androidBinding).toContain('RuntimeExecutor')
    expect(nativeBinding).toContain('retainUint8Array')
    expect(nativeBinding).toContain('submit')
    expect(nativeBinding).toContain('dispatchCommandToAndroid')
    expect(nativeBinding).toContain('__unifiedBleNativeProtocolV2')
    expect(runtime).toContain('retainUint8Array')
    expect(runtime).toContain('copyBinary')
    expect(runtime).toContain('releaseBinary')
    expect(runtime).toContain('registerCommand')
    expect(runtime).toContain('settleResult')
    expect(runtime).toContain('deliverUint8ArrayCopy')
    expect(nativeBinding).not.toMatch(/Base64/)
  })

  test('keeps Android PHY runtime truth in the versioned handshake extension without changing protocol-v2 records', () => {
    const control = read('src/NativeUnifiedBleProtocolControl.ts')
    const androidControl = read(
      'android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolControlModule.java'
    )
    const dispatcher = read(
      'android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolAndroidDispatcher.kt'
    )
    const schema = read('src/native-protocol/generated/native-protocol-v2-schema.ts')

    expect(control).toContain('phyAvailable?: boolean')
    expect(androidControl).toContain('phyAvailable')
    expect(dispatcher).toContain('Build.VERSION_CODES.O')
    expect(schema).toContain('export const NATIVE_PROTOCOL_VERSION = 2')
    expect(schema).not.toContain('phyAvailable')
  })

  test('versions Android link-control schema additions with ABI v3 while retaining protocol v2 records', () => {
    const schema = JSON.parse(fs.readFileSync(schemaPath, 'utf8'))
    const manifest = JSON.parse(fs.readFileSync(path.join(path.dirname(schemaPath), schema.abiManifest), 'utf8'))
    const cpp = read('native/protocol/generated/NativeProtocolV2Schema.hpp')
    const kotlin = read(
      'android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/generated/NativeProtocolV2Schema.kt'
    )
    const swift = read('ios/Generated/NativeProtocolV2Schema.swift')
    const typescript = read('src/native-protocol/generated/native-protocol-v2-schema.ts')
    const androidControl = read(
      'android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolControlModule.java'
    )

    expect(schema.version).toBe(2)
    expect(schema.abiVersion).toBe(3)
    expect(manifest.version).toBe(3)
    expect(cpp).toContain('kProtocolVersion = 2U')
    expect(cpp).toContain('kAbiVersion = 3U')
    expect(kotlin).toContain('NATIVE_PROTOCOL_VERSION: Int = 2')
    expect(kotlin).toContain('NATIVE_PROTOCOL_ABI_VERSION: Int = 3')
    expect(swift).toContain('nativeProtocolVersion: UInt32 = 2')
    expect(swift).toContain('nativeProtocolABIVersion: UInt32 = 3')
    expect(typescript).toContain('export const NATIVE_PROTOCOL_VERSION = 2')
    expect(typescript).toContain('export const NATIVE_PROTOCOL_ABI_VERSION = 3')
    expect(androidControl).toContain('NativeProtocolV2SchemaKt.NATIVE_PROTOCOL_ABI_VERSION')
  })

  test('keeps Android JNI advertisement bytes and rejected command input under explicit native ownership', () => {
    const androidBinding = read('android/src/main/jni/UnifiedBleProtocolJsiBinding.cpp')
    const nativeRuntime = read('native/protocol/src/NativeProtocolControlRuntime.cpp')
    const androidBoundary = read('src/native-protocol/rn-android-boundary.ts')

    expect(androidBinding).toContain('void emitAdvertisementFromJava(')
    expect(androidBinding).toContain('activeRuntime->retainNativeBytes(')
    expect(androidBinding).toContain('rawRecord:androidBluetoothLe')
    expect(androidBoundary).toContain('const advertisementBytes = this.takeAdvertisementBytes(parsedAdvertisement)')
    expect(androidBoundary).toContain('advertisementFromRecord(parsedAdvertisement, advertisementBytes)')
    expect(androidBinding).toContain('activeRuntime->rejectCommandDispatch(command)')
    expect(nativeRuntime).toContain('bool NativeProtocolControlRuntime::rejectCommandDispatch(')
    expect(nativeRuntime).toContain('NativeOperationState::failed')
    expect(nativeRuntime).toContain('binaryTransport_->release(*inputBinary)')
    expect(nativeRuntime.indexOf('operations_->registerOperation(operation, cancellable);')).toBeLessThan(
      nativeRuntime.indexOf('binaryTransport_->copyForNative(')
    )
  })

  test('carries every public Android ScanRecord advertisement field through the generated protocol records', () => {
    const radio = read('android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt')
    const dispatcher = read(
      'android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolAndroidDispatcher.kt'
    )
    const javaBinding = read(
      'android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolJsiBinding.java'
    )
    const jniBinding = read('android/src/main/jni/UnifiedBleProtocolJsiBinding.cpp')

    expect(radio).toContain('data class OwnedAndroidProtocolAdvertisement')
    expect(radio).toContain('AndroidScanResult.TX_POWER_NOT_PRESENT')
    expect(radio).toContain('scanRecord?.txPowerLevel')
    expect(radio).toContain('result.isConnectable')
    expect(radio).toContain('Build.VERSION_CODES.Q')
    expect(radio).toContain('serviceSolicitationUuids')
    expect(radio).toContain('ScanRecord.DATA_TYPE_APPEARANCE')
    expect(radio).toContain('Build.VERSION_CODES.TIRAMISU')
    expect(radio).toContain('scanRecord?.serviceData')
    expect(radio).toContain('manufacturerSpecificData')
    expect(dispatcher).toContain('advertisement.serviceData')
    expect(dispatcher).toContain('advertisement.manufacturerData')
    expect(javaBinding).toContain('boolean hasTxPower')
    expect(javaBinding).toContain('int connectableState')
    expect(javaBinding).toContain('boolean hasAppearance')
    expect(javaBinding).toContain('String[] solicitedServiceUuids')
    expect(javaBinding).toContain('byte[][] serviceDataValues')
    expect(javaBinding).toContain('int[] manufacturerCompanyIdentifiers')
    expect(jniBinding).toContain('RecordKind::serviceDataEntry')
    expect(jniBinding).toContain('RecordKind::manufacturerDataEntry')
    expect(jniBinding).toContain('protocolField(7U, static_cast<std::int64_t>(txPower))')
    expect(jniBinding).toContain('protocolField(9U, static_cast<std::uint64_t>(appearance))')
    expect(jniBinding).toContain('protocolField(11U, *solicitedUuids)')
    expect(jniBinding).toContain('protocolField(13U, std::move(serviceDataEntries))')
    expect(jniBinding).toContain('protocolField(14U, std::move(manufacturerDataEntries))')
    expect(jniBinding).not.toMatch(/Base64/)
  })

  test('gates additive Android security events and rejects hidden bond APIs', () => {
    const dispatcher = read(
      'android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolAndroidDispatcher.kt'
    )
    const radio = read('android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt')
    const boundary = read('src/native-protocol/rn-android-boundary.ts')
    expect(dispatcher).toContain('securityEventsEnabled')
    expect(dispatcher).toContain('if (securityEventsEnabled.get()) emitSecurityStateChanged')
    expect(dispatcher).toContain('securityEventsEnabled.set(true)')
    expect(dispatcher).toContain('securityEventsEnabled.set(false)\n    radio.onSecurityState = null')
    expect(dispatcher).toContain('bondStateUnknown')
    expect(radio).toContain('catch (error: SecurityException)')
    expect(radio).toContain(
      'internal var onSecurityState: ((deviceId: String, state: OwnedAndroidSecurityState) -> Unit)?'
    )
    expect(radio).toContain('else -> "unknown"')
    expect(radio).not.toContain('cancelBondProcess')
    expect(radio).not.toContain('removeBond')
    expect(boundary).toContain('signal: AbortSignal | null = null')
    expect(boundary).toContain('if (isAbortSignalAborted(signal))')
  })

  test('represents complete paths, rich advertisements, errors, cancellation, and restoration', () => {
    const schema = JSON.parse(fs.readFileSync(schemaPath, 'utf8'))
    const records = new Map(schema.records.map(record => [record.name, record.fields.map(field => field[0])]))
    expect(records.get('characteristicPath')).toEqual(['service', 'characteristicUuid', 'characteristicOccurrence'])
    expect(records.get('descriptorPath')).toEqual(['characteristic', 'descriptorUuid', 'descriptorOccurrence'])
    expect(records.get('operationCorrelation')).toEqual(['attachment', 'dispatchEpoch', 'nonce'])
    expect(records.get('advertisement')).toEqual(
      expect.arrayContaining([
        'serviceUuids',
        'solicitedServiceUuids',
        'overflowServiceUuids',
        'serviceData',
        'manufacturerData',
        'rawRecord',
        'scanResponseRecord',
        'fieldProvenance'
      ])
    )
    expect(records.get('error')).toEqual(
      expect.arrayContaining(['androidGattStatus', 'coreBluetoothDomain', 'coreBluetoothCode', 'safeMetadata'])
    )
    expect(records.get('restorationAdoptionResult')).toEqual(
      expect.arrayContaining(['receiptId', 'outcome', 'records'])
    )
    expect(records.get('scanOptions')).toEqual(
      expect.arrayContaining(['serviceUuids', 'allowDuplicates', 'scanMode', 'callbackType', 'legacyScan'])
    )
    expect(records.get('databaseSnapshot')).toEqual(
      expect.arrayContaining(['databasePath', 'services', 'characteristics', 'descriptors'])
    )
    expect(records.get('adapterStateSnapshot')).toEqual(
      expect.arrayContaining(['availability', 'authorization', 'power', 'safeReason'])
    )
    expect(records.get('event')).toEqual(expect.arrayContaining(['adapterState']))
    expect(schema.adapterAvailability).toEqual(expect.arrayContaining(['available', 'unsupported', 'unknown']))
    expect(schema.adapterAuthorization).toEqual(expect.arrayContaining(['granted', 'denied', 'notDetermined']))
    expect(schema.adapterPower).toEqual(expect.arrayContaining(['on', 'off', 'resetting', 'unknown']))
    expect(records.get('command')).toEqual(expect.arrayContaining(['scanOptions', 'writeMode']))
    expect(records.get('result')).toEqual(expect.arrayContaining(['databaseSnapshot']))
    expect(schema.commandKinds).toEqual(expect.arrayContaining(['cancel', 'subscribe', 'unsubscribe']))
  })
})

function listFiles(directory) {
  const files = []
  for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
    const entryPath = path.join(directory, entry.name)
    if (entry.isDirectory()) {
      files.push(...listFiles(entryPath))
    } else if (entry.isFile()) {
      files.push(entryPath)
    }
  }
  return files
}
