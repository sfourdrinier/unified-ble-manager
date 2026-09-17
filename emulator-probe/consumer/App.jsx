// emulator-probe/consumer/App.jsx
// Minimal probe UI driving the REAL 4.x native module
// (createReactNativeBleManager -> UnifiedBleProtocolControl) on the emulator.
// Every outcome is reported to the status text AND to logcat via the
// [UBM_PROBE] tag so the adb battery can assert on it.

import React, { useEffect, useRef, useState } from 'react'
import { Button, PermissionsAndroid, Platform, ScrollView, Text, View } from 'react-native'
import { createReactNativeBleManager } from 'unified-ble-manager/react-native'
import { createReactNativeRustCoreBinding } from '@ubm-rustcore-producer'

function say(setStatus, line) {
  const tagged = `[UBM_PROBE] ${line}`
  console.log(tagged)
  setStatus(prev => `${prev}\n${tagged}`)
}

async function ensurePermission(setStatus) {
  if (Platform.OS !== 'android') return true
  if (Platform.Version < 31) {
    const location = await PermissionsAndroid.request(
      PermissionsAndroid.PERMISSIONS.ACCESS_FINE_LOCATION,
    )
    say(setStatus, `location=${location}`)
    return location === PermissionsAndroid.RESULTS.GRANTED
  }
  const result = await PermissionsAndroid.requestMultiple([
    PermissionsAndroid.PERMISSIONS.BLUETOOTH_SCAN,
    PermissionsAndroid.PERMISSIONS.BLUETOOTH_CONNECT,
  ])
  const scan = result[PermissionsAndroid.PERMISSIONS.BLUETOOTH_SCAN]
  const connect = result[PermissionsAndroid.PERMISSIONS.BLUETOOTH_CONNECT]
  say(setStatus, `scan=${scan} connect=${connect}`)
  return scan === PermissionsAndroid.RESULTS.GRANTED && connect === PermissionsAndroid.RESULTS.GRANTED
}

export function App() {
  const [status, setStatus] = useState('[UBM_PROBE] ready')
  const managerRef = useRef(null)
  const [busy, setBusy] = useState(false)

  useEffect(() => {
    say(setStatus, 'app-mounted')
  }, [])

  async function onInit() {
    if (busy) return
    setBusy(true)
    try {
      const ok = await ensurePermission(setStatus)
      say(setStatus, `permission-ok=${ok}`)
      const manager = await createReactNativeBleManager()
      managerRef.current = manager
      say(setStatus, 'manager-created')
      const adapter = await manager.adapter.state()
      say(setStatus, `adapter power=${adapter.power} availability=${adapter.availability} authorization=${adapter.authorization}`)
    } catch (err) {
      say(setStatus, `init-error name=${err?.name} code=${err?.code} message=${String(err?.message)}`)
    } finally {
      setBusy(false)
    }
  }

  async function onScanCancel() {
    if (busy) return
    setBusy(true)
    try {
      const manager = managerRef.current ?? (await createReactNativeBleManager())
      managerRef.current = manager
      const controller = new AbortController()
      const timer = setTimeout(() => {
        say(setStatus, 'abort-requested')
        controller.abort()
      }, 1500)
      try {
        const peer = await manager.find({
          query: { anyOf: [{ services: { any: ['180D'] } }] },
          timeoutMs: 8000,
          select: 'first',
          signal: controller.signal,
        })
        clearTimeout(timer)
        say(setStatus, `find-resolved peer=${JSON.stringify(peer).slice(0, 120)}`)
      } catch (err) {
        clearTimeout(timer)
        say(setStatus, `find-settled name=${err?.name} code=${err?.code} message=${String(err?.message)}`)
      }
    } catch (err) {
      say(setStatus, `scancancel-error name=${err?.name} code=${err?.code} message=${String(err?.message)}`)
    } finally {
      setBusy(false)
    }
  }

  async function onBonded() {
    if (busy) return
    setBusy(true)
    try {
      const manager = managerRef.current ?? (await createReactNativeBleManager())
      managerRef.current = manager
      const peers = await manager.peers.bonded()
      say(setStatus, `bonded-count=${peers.length}`)
    } catch (err) {
      say(setStatus, `bonded-error name=${err?.name} code=${err?.code} message=${String(err?.message)}`)
    } finally {
      setBusy(false)
    }
  }

  async function onAbortedFind() {
    if (busy) return
    setBusy(true)
    try {
      const manager = managerRef.current ?? (await createReactNativeBleManager())
      managerRef.current = manager
      const controller = new AbortController()
      controller.abort()
      say(setStatus, 'abort-requested-before-dispatch')
      try {
        await manager.find({
          query: { anyOf: [{ services: { any: ['180D'] } }] },
          timeoutMs: 8000,
          select: 'first',
          signal: controller.signal,
        })
        say(setStatus, 'aborted-find-resolved-unexpectedly')
      } catch (err) {
        say(setStatus, `aborted-find-settled name=${err?.name} code=${err?.code} message=${String(err?.message)}`)
      }
    } catch (err) {
      say(setStatus, `abortedfind-error message=${String(err?.message)}`)
    } finally {
      setBusy(false)
    }
  }

  async function onRustCore() {
    if (busy) return
    setBusy(true)
    try {
      const binding = createReactNativeRustCoreBinding()
      const session = await binding.openSession('ubm-probe')
      const contractRevision = session.contractRevision()
      const status = await session.invoke('central.status', {})
      const echo = await session.invoke('echo.counter', { decimal: '41' })
      await session.close()
      say(setStatus, `rustcore-ok contract=${contractRevision} status=${JSON.stringify(status)} echo=${echo} closed=true`)
    } catch (err) {
      say(setStatus, `rustcore-error name=${err?.name} code=${err?.code} message=${String(err?.message)}`)
    } finally {
      setBusy(false)
    }
  }

  async function onTeardown() {
    try {
      await managerRef.current?.destroy()
      managerRef.current = null
      say(setStatus, 'manager-destroyed')
    } catch (err) {
      say(setStatus, `teardown-error message=${String(err?.message)}`)
    }
  }

  return (
    <View style={{ flex: 1, padding: 16, justifyContent: 'flex-start' }}>
      <Text testID="probeTitle" style={{ fontSize: 20, marginBottom: 12 }}>UBM5 emulator probe</Text>
      <Button testID="initButton" title="Init manager" onPress={onInit} />
      <View style={{ height: 8 }} />
      <Button testID="scanCancelButton" title="Scan 8s then cancel" onPress={onScanCancel} />
      <View style={{ height: 8 }} />
      <Button testID="bondedButton" title="Bonded peers" onPress={onBonded} />
      <View style={{ height: 8 }} />
      <Button testID="abortedFindButton" title="Aborted find" onPress={onAbortedFind} />
      <View style={{ height: 8 }} />
      <Button testID="rustCoreButton" title="RustCore session" onPress={onRustCore} />
      <View style={{ height: 8 }} />
      <Button testID="teardownButton" title="Teardown" onPress={onTeardown} />
      <ScrollView style={{ marginTop: 12 }}>
        <Text testID="statusText">{status}</Text>
      </ScrollView>
    </View>
  )
}
