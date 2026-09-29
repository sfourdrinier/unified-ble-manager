// examples-shared/driver/index.ts
//
// What a host adapter imports. Hosts depend on this surface only; the server
// side (server/*.mjs) imports protocol.ts directly.

export * from './create-driver.ts'
export * from './driver-url.ts'
export * from './host.ts'
export * from './protocol.ts'
export * from './remote-channel.ts'
export * from './scenario-core.ts'
export * from './user-gesture.ts'
export { toJsonObject } from './scenarios/ble-scenario.ts'
export { isPmdRecording } from './pmd-recording.ts'
export { H10_ACC_SAMPLE_RATES_HZ, H10_ACC_RANGES_G } from './polar-pmd.ts'
export type { H10AccSampleRateHz, H10AccRangeG } from './polar-pmd.ts'
