// src/backends/reactnative/react-native-rust-core.ts
//
// F01 React Native shared-core seam: selection, binding resolution, and
// contract admission between the RN factory layer and the native Rust core
// (the future JNI/UniFFI op surface; both bindings are echo-only today, so
// no factory constructs through this seam yet).
//
// The seam never constructs (or falls back to) the TypeScript manager: a
// missing binding or a foreign revision fails loudly, so the F01
// acceptance proof can never silently exercise the old TS runtime. The
// follow-up composes these primitives into a binding-backed provider once
// the native op surface lands; the dispatched ops below are the exact F01
// slice it must route (scan/connect/subscribe/disconnect/dispose).

import { contractError } from '../../backend-contract/errors'

/**
 * The linked `ubm-core` contract revision a native Rust core must report.
 * Pinned like the Tauri compatibility entry; the RN seam suite asserts it
 * still equals the frozen contracts revision.
 */
export const RUST_CORE_CONTRACT_REVISION = 'C-UBM.0.1.2-DRAFT'

/** One admitted native Rust core session. Implemented by the native module. */
export interface ReactNativeRustCoreSession {
  /** The linked `ubm-core` contract revision this session executes. */
  contractRevision(): string
  /**
   * Invoke one core op. The seam interprets nothing: args cross verbatim
   * and the raw core result returns. No TS scheduling, subscription, or
   * timeout state lives here.
   */
  invoke(op: string, args: Record<string, unknown>): Promise<unknown>
  /** Release the session. Idempotent. */
  close(): Promise<void>
}

/** Native Rust core entry injected by the host application. */
export interface ReactNativeRustCoreBinding {
  openSession(owner: string): Promise<ReactNativeRustCoreSession>
}

function isRustCoreSession(candidate: unknown): candidate is ReactNativeRustCoreSession {
  if (typeof candidate !== 'object' || candidate === null) return false
  const session = candidate as Record<string, unknown>
  return (
    typeof session.contractRevision === 'function' &&
    typeof session.invoke === 'function' &&
    typeof session.close === 'function'
  )
}

/**
 * Resolve the injected native binding. Anything but a well-shaped binding
 * fails with `capability.unsupported`: there is no silent TypeScript
 * fallback, by design.
 */
export function resolveReactNativeRustCoreBinding(candidate: unknown): ReactNativeRustCoreBinding {
  if (typeof candidate !== 'object' || candidate === null) {
    throw contractError('capability.unsupported', 'capability', 'react-native-manager.rust-core-missing')
  }
  const binding = (candidate as { openSession?: unknown }).openSession
  if (typeof binding !== 'function') {
    throw contractError('capability.unsupported', 'capability', 'react-native-manager.rust-core-missing')
  }
  return candidate as ReactNativeRustCoreBinding
}

/**
 * Admit one open session: its reported revision must equal the pinned
 * shared-core revision, else `protocol.incompatible`. A session that fails
 * the shape check fails as a missing core, never as an implicit pass.
 */
export async function admitReactNativeRustCoreSession(session: unknown): Promise<ReactNativeRustCoreSession> {
  if (!isRustCoreSession(session)) {
    throw contractError('capability.unsupported', 'capability', 'react-native-manager.rust-core-missing')
  }
  if (session.contractRevision() !== RUST_CORE_CONTRACT_REVISION) {
    throw contractError('protocol.incompatible', 'core', 'react-native-manager.rust-core-revision')
  }
  return session
}

/** Open one session on the binding and admit its contract revision. */
export async function openAdmittedRustCoreSession(
  binding: ReactNativeRustCoreBinding,
  owner: string
): Promise<ReactNativeRustCoreSession> {
  if (owner.length === 0) {
    throw contractError('argument.invalid', 'core', 'react-native-manager.rust-core-owner')
  }
  return admitReactNativeRustCoreSession(await binding.openSession(owner))
}

/**
 * Dispatch one op through an admitted session. The op name must be
 * non-empty; everything else crosses untouched.
 */
export async function dispatchReactNativeRustCoreOp(
  session: ReactNativeRustCoreSession,
  op: string,
  args: Record<string, unknown>
): Promise<unknown> {
  if (op.length === 0) {
    throw contractError('argument.invalid', 'core', 'react-native-manager.rust-core-op')
  }
  return session.invoke(op, args)
}
