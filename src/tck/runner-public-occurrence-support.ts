// src/tck/runner-public-occurrence-support.ts
//
// One reading of the discovery contract (docs/UNIFIED_SEMANTICS.md §9) for
// every scenario that inspects a snapshot: a service path is (service UUID,
// service occurrence); a characteristic adds (characteristic UUID,
// characteristic occurrence); a descriptor adds (descriptor UUID, descriptor
// occurrence). The occurrence is the index of a repeated UUID under its
// parent, in discovery order, so a key that omits the UUID at any level
// conflates distinct attributes.

import type { SerializableRecord } from '../backend-contract/primitives'

interface ServicePathKeyFields {
  readonly serviceUuid: unknown
  readonly serviceOccurrence: unknown
}

interface CharacteristicPathKeyFields extends ServicePathKeyFields {
  readonly characteristicUuid: unknown
  readonly characteristicOccurrence: unknown
}

interface DescriptorPathKeyFields extends CharacteristicPathKeyFields {
  readonly descriptorUuid: unknown
  readonly descriptorOccurrence: unknown
}

/** The attribute shape of a discovery snapshot this module reads. */
export interface OccurrenceSnapshot {
  readonly services: readonly { readonly path: ServicePathKeyFields }[]
  readonly characteristics: readonly {
    readonly path: CharacteristicPathKeyFields
    readonly properties: { readonly notify: boolean }
  }[]
  readonly descriptors: readonly { readonly path: DescriptorPathKeyFields }[]
}

export function servicePathKey(path: ServicePathKeyFields): string {
  return JSON.stringify([String(path.serviceUuid), String(path.serviceOccurrence)])
}

export function characteristicPathKey(path: CharacteristicPathKeyFields): string {
  return JSON.stringify([
    String(path.serviceUuid),
    String(path.serviceOccurrence),
    String(path.characteristicUuid),
    String(path.characteristicOccurrence)
  ])
}

export function descriptorPathKey(path: DescriptorPathKeyFields): string {
  return JSON.stringify([
    String(path.serviceUuid),
    String(path.serviceOccurrence),
    String(path.characteristicUuid),
    String(path.characteristicOccurrence),
    String(path.descriptorUuid),
    String(path.descriptorOccurrence)
  ])
}

/** What a snapshot's occurrence indexing looks like, level by level. */
export interface OccurrenceIndexing {
  /** Every complete path is unique at its level. */
  readonly pathsUnique: boolean
  /** Every characteristic names a discovered service; every descriptor a discovered characteristic. */
  readonly parentsResolve: boolean
  /** Each repeated UUID under one parent is numbered 0, 1, 2, … in discovery order. */
  readonly occurrencesExact: boolean
  readonly distinctServiceUuids: number
  readonly maximumServiceOccurrence: number
  readonly maximumCharacteristicOccurrence: number
  readonly maximumDescriptorOccurrence: number
}

export function inspectOccurrenceIndexing(snapshot: OccurrenceSnapshot): OccurrenceIndexing {
  const serviceKeys = snapshot.services.map(service => servicePathKey(service.path))
  const characteristicKeys = snapshot.characteristics.map(characteristic => characteristicPathKey(characteristic.path))
  const descriptorKeys = snapshot.descriptors.map(descriptor => descriptorPathKey(descriptor.path))
  const services = new Set(serviceKeys)
  const characteristics = new Set(characteristicKeys)
  const pathsUnique =
    services.size === serviceKeys.length &&
    characteristics.size === characteristicKeys.length &&
    new Set(descriptorKeys).size === descriptorKeys.length
  const parentsResolve =
    snapshot.characteristics.every(characteristic => services.has(servicePathKey(characteristic.path))) &&
    snapshot.descriptors.every(descriptor => characteristics.has(characteristicPathKey(descriptor.path)))
  const occurrencesExact =
    indexedPerParent(
      snapshot.services.map(service => ['', service.path.serviceUuid, service.path.serviceOccurrence])
    ) &&
    indexedPerParent(
      snapshot.characteristics.map(characteristic => [
        servicePathKey(characteristic.path),
        characteristic.path.characteristicUuid,
        characteristic.path.characteristicOccurrence
      ])
    ) &&
    indexedPerParent(
      snapshot.descriptors.map(descriptor => [
        characteristicPathKey(descriptor.path),
        descriptor.path.descriptorUuid,
        descriptor.path.descriptorOccurrence
      ])
    )
  return Object.freeze({
    pathsUnique,
    parentsResolve,
    occurrencesExact,
    distinctServiceUuids: new Set(snapshot.services.map(service => String(service.path.serviceUuid))).size,
    maximumServiceOccurrence: maximumOccurrence(snapshot.services.map(service => service.path.serviceOccurrence)),
    maximumCharacteristicOccurrence: maximumOccurrence(
      snapshot.characteristics.map(characteristic => characteristic.path.characteristicOccurrence)
    ),
    maximumDescriptorOccurrence: maximumOccurrence(
      snapshot.descriptors.map(descriptor => descriptor.path.descriptorOccurrence)
    )
  })
}

/**
 * The world the duplicate-UUID scenario needs to observe anything: a second
 * service UUID, and a same-UUID sibling (occurrence ≥ 1) at the service,
 * characteristic and descriptor levels.
 */
export function occurrenceWorldIsComplete(indexing: OccurrenceIndexing): boolean {
  return (
    indexing.distinctServiceUuids >= 2 &&
    indexing.maximumServiceOccurrence >= 1 &&
    indexing.maximumCharacteristicOccurrence >= 1 &&
    indexing.maximumDescriptorOccurrence >= 1
  )
}

export function occurrenceIndexingDetail(indexing: OccurrenceIndexing): SerializableRecord {
  return Object.freeze({ ...indexing })
}

/**
 * The notifiable characteristics a UUID alone cannot address: those whose
 * (service UUID, characteristic UUID) pair names more than one instance.
 * Delivering to each by its occurrence proves the backend routes by the
 * complete path, not by UUID.
 */
export function ambiguousNotifiableCharacteristics<
  Characteristic extends OccurrenceSnapshot['characteristics'][number]
>(characteristics: readonly Characteristic[]): readonly Characteristic[] {
  const instances = new Map<string, number>()
  for (const characteristic of characteristics) {
    const pair = uuidPairKey(characteristic.path)
    instances.set(pair, (instances.get(pair) ?? 0) + 1)
  }
  return characteristics.filter(
    characteristic => characteristic.properties.notify && (instances.get(uuidPairKey(characteristic.path)) ?? 0) > 1
  )
}

/** True when the targets differ both by characteristic occurrence and by service occurrence. */
export function routingTargetsCoverBothLevels(
  targets: readonly { readonly path: CharacteristicPathKeyFields }[]
): boolean {
  const differsBy = (level: 'service' | 'characteristic') =>
    targets.some(left =>
      targets.some(
        right =>
          uuidPairKey(left.path) === uuidPairKey(right.path) &&
          (level === 'service'
            ? String(left.path.serviceOccurrence) !== String(right.path.serviceOccurrence)
            : String(left.path.serviceOccurrence) === String(right.path.serviceOccurrence) &&
              String(left.path.characteristicOccurrence) !== String(right.path.characteristicOccurrence))
      )
    )
  return differsBy('service') && differsBy('characteristic')
}

function uuidPairKey(path: CharacteristicPathKeyFields): string {
  return JSON.stringify([String(path.serviceUuid), String(path.characteristicUuid)])
}

function indexedPerParent(entries: readonly (readonly [string, unknown, unknown])[]): boolean {
  const next = new Map<string, number>()
  for (const [parent, uuid, occurrence] of entries) {
    const group = JSON.stringify([parent, String(uuid)])
    const expected = next.get(group) ?? 0
    if (String(occurrence) !== String(expected)) {
      return false
    }
    next.set(group, expected + 1)
  }
  return true
}

function maximumOccurrence(occurrences: readonly unknown[]): number {
  return occurrences.reduce<number>((maximum, occurrence) => {
    const index = Number(String(occurrence))
    return Number.isSafeInteger(index) && index > maximum ? index : maximum
  }, -1)
}
