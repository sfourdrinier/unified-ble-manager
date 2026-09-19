// src/backend-contract/event-vocabulary.ts
//
// One name per physical event on every backend (owner decision, 5.0): the
// error code, lifecycle transition and stream terminal an application sees
// for the same thing happening on the radio, and the connection
// supervisor's decision for it. Android is the reference wherever a
// platform can do the same; a backend appears under `differs` only where
// its platform genuinely cannot tell, with the reason. The platform's own
// answer (GATT status, NSError, HRESULT, D-Bus error) always stays in the
// error's `platform` detail.
//
// Pinned three ways: `docs/UNIFIED_SEMANTICS.md` embeds `renderEventVocabularyTable()`,
// `crates/ubm-desktop/tests/fixtures/event-vocabulary.json` is this table
// for the Rust mapping tests (desktop and mobile), and the Web and
// supervisor tests read it (`__tests__/event-vocabulary.test.js`).

import type { ConnectionState } from './backend'
import type { ConnectionLifecycleTerminalCause } from './connection-lifecycle'
import type { BleErrorCode, BleRetryability } from './errors'
import type { StreamTerminalNotice } from './streams'

export const VOCABULARY_BACKENDS = Object.freeze([
  'react-native-android',
  'react-native-ios',
  'desktop-macos',
  'desktop-windows',
  'desktop-linux',
  'web'
] as const)
export type VocabularyBackend = (typeof VOCABULARY_BACKENDS)[number]

/** Where the supervisor meets the event: a connect attempt, `configure`, the lifecycle of a live link, or a restored peer. */
export type SupervisorContext = 'connect' | 'configure' | 'lifecycle' | 'restore'

/** What `createConnectionSupervisor` does about the event, on every host. */
export type SupervisorDecision = 'reconnect' | 'wait-for-adapter' | 'stop'

export interface EventNames {
  /** The error code an operation reports, or `null` when no operation fails. */
  readonly error: BleErrorCode | null
  /** The retryability that error reports. */
  readonly retryability: BleRetryability | null
  /** The connection lifecycle transition, or `null` when the link is unaffected. */
  readonly lifecycle: { readonly current: ConnectionState; readonly cause: ConnectionLifecycleTerminalCause } | null
  /** How a notification stream on the link ends, or `null`. */
  readonly streamTerminal: StreamTerminalNotice['reason'] | null
}

export interface PhysicalEventEntry {
  readonly event: string
  readonly description: string
  readonly names: EventNames
  readonly supervisor: { readonly context: SupervisorContext; readonly decision: SupervisorDecision }
  /** Backends whose platform genuinely cannot report the reference names. */
  readonly differs: Readonly<
    Partial<Record<VocabularyBackend, { readonly names: Partial<EventNames>; readonly why: string }>>
  >
}

const LINK_LOST = Object.freeze({ current: 'lost', cause: 'peer-link-loss' } as const)
const RELEASED = Object.freeze({ current: 'disconnected', cause: 'requested-disconnect' } as const)
const ADAPTER_LOST = Object.freeze({ current: 'lost', cause: 'adapter-loss' } as const)

function names(partial: Partial<EventNames>): EventNames {
  return Object.freeze({ error: null, retryability: null, lifecycle: null, streamTerminal: null, ...partial })
}

export const EVENT_VOCABULARY: readonly PhysicalEventEntry[] = Object.freeze([
  {
    event: 'link-lost',
    description:
      'The link dropped while idle: the peer went away or out of range, the remote side terminated, or the supervision timeout expired.',
    names: names({ lifecycle: LINK_LOST, streamTerminal: 'connection-lost' }),
    supervisor: { context: 'lifecycle', decision: 'reconnect' },
    differs: {}
  },
  {
    event: 'link-lost-during-operation',
    description:
      'The link dropped while an operation (discovery, read, write, subscribe) was pending. The operation ends at once, even when the radio never answers it.',
    names: names({
      error: 'connection.lost',
      retryability: 'never',
      lifecycle: LINK_LOST,
      streamTerminal: 'connection-lost'
    }),
    supervisor: { context: 'configure', decision: 'reconnect' },
    differs: {}
  },
  {
    event: 'requested-disconnect',
    description: 'The app released or disconnected the link. A supervisor whose link the app released reconnects.',
    names: names({ lifecycle: RELEASED, streamTerminal: 'owner-released' }),
    supervisor: { context: 'lifecycle', decision: 'reconnect' },
    differs: {}
  },
  {
    event: 'requested-disconnect-during-operation',
    description: "The app's own release cut off a pending operation.",
    names: names({
      error: 'operation.disconnected',
      retryability: 'never',
      lifecycle: RELEASED,
      streamTerminal: 'owner-released'
    }),
    supervisor: { context: 'configure', decision: 'stop' },
    differs: {}
  },
  {
    event: 'adapter-loss',
    description: 'Bluetooth was turned off, reset, removed or revoked while a link was up.',
    names: names({ lifecycle: ADAPTER_LOST, streamTerminal: 'source-failed' }),
    supervisor: { context: 'lifecycle', decision: 'wait-for-adapter' },
    differs: {}
  },
  {
    event: 'adapter-loss-during-operation',
    description: 'The adapter went away while an operation was pending.',
    names: names({
      error: 'operation.reset',
      retryability: 'never',
      lifecycle: ADAPTER_LOST,
      streamTerminal: 'source-failed'
    }),
    supervisor: { context: 'configure', decision: 'wait-for-adapter' },
    differs: {}
  },
  {
    event: 'connect-not-established',
    description:
      'The platform could not establish the link (Android GATT 133/62/147, CoreBluetooth connectionFailed/connectionTimeout, WinRT Unreachable, BlueZ Failed/ConnectionAttemptFailed, Web NetworkError). Nothing was committed; the library never retries it itself.',
    names: names({ error: 'connection.failed', retryability: 'caller-decides' }),
    supervisor: { context: 'connect', decision: 'reconnect' },
    differs: {}
  },
  {
    event: 'connect-deadline-expired',
    description:
      'The dispatched connect deadline expired before any link came up: the peer did not answer the attempt within the bound. The controller giving up (Android GATT 133/62/147) is the same physical event under another observation — CoreBluetooth never fails a pending connect on its own, nor do btleplug and Web report the expiry, so the deadline is their only answer. Nothing was committed; the library never retries it itself. A caller-supplied AbortSignal abort stays `operation.aborted`. The deadline fact rides in the error platform detail.',
    names: names({ error: 'connection.failed', retryability: 'caller-decides' }),
    supervisor: { context: 'connect', decision: 'reconnect' },
    differs: {}
  },
  {
    event: 'peer-not-found',
    description: 'The peer was never observed (or chosen, on Web), so there is nothing to connect to.',
    names: names({ error: 'peer.not-found', retryability: 'never' }),
    supervisor: { context: 'connect', decision: 'stop' },
    differs: {}
  },
  {
    event: 'security-refused',
    description:
      'The peer refused an operation for lack of authentication, authorization or encryption (ATT 0x05/0x08/0x0C/0x0F, Android 137, CoreBluetooth peerRemovedPairingInformation/encryptionTimedOut, BlueZ NotAuthorized/"Not paired", Web SecurityError). Recovery: pair or repair.',
    names: names({ error: 'platform.security', retryability: 'never' }),
    supervisor: { context: 'configure', decision: 'stop' },
    differs: {}
  },
  {
    event: 'operation-timed-out',
    description:
      "The operation's deadline expired before the platform answered — except a dispatched connect, whose deadline expiring before any link came up is `connect-deadline-expired` (one name for the peer not answering).",
    names: names({ error: 'operation.timed-out', retryability: 'caller-decides' }),
    supervisor: { context: 'configure', decision: 'stop' },
    differs: {}
  },
  {
    event: 'operation-cancelled',
    description: 'The caller aborted the operation before the platform answered.',
    names: names({ error: 'operation.aborted', retryability: 'caller-decides' }),
    supervisor: { context: 'configure', decision: 'stop' },
    differs: {}
  },
  {
    event: 'restoration-received',
    description:
      'The OS handed back known peers after the app was gone: iOS relaunched the app on a BLE event and delivered restored peripherals through `willRestoreState`; Android woke the process through Companion Device Manager device presence (API 31+) for an armed associated peer. The library surfaces the same restored peer records on both phones (`peers.restored`, `restoration-received`, `restoration.claim()`). No operation failed and no link transitioned yet — the app reconnects through the public `connect` (Android `when-available`, a restored iOS link the OS still holds completing at once) and replays subscriptions through `subscribe`. Presence observation below API 31 has no wake; it reports `capability.unsupported`.',
    names: names({}),
    supervisor: { context: 'restore', decision: 'reconnect' },
    differs: {
      'desktop-macos': {
        names: {},
        why: 'macOS has no OS restoration journal for a terminated app and no presence wake; the event never fires and presence observation reports `capability.unsupported` with a reason.'
      },
      'desktop-windows': {
        names: {},
        why: 'Windows has no OS restoration journal for a terminated app and no presence wake; the event never fires and presence observation reports `capability.unsupported` with a reason.'
      },
      'desktop-linux': {
        names: {},
        why: 'Linux has no OS restoration journal for a terminated app and no presence wake; the event never fires and presence observation reports `capability.unsupported` with a reason.'
      },
      web: {
        names: {},
        why: 'Web Bluetooth has no background relaunch or presence wake; the event never fires and restoration reports `capability.unsupported` with a reason.'
      }
    }
  }
] satisfies readonly PhysicalEventEntry[])

/** The public names `backend` reports for `event`: the reference names unless the platform genuinely differs. */
export function eventNamesFor(event: string, backend: VocabularyBackend): EventNames {
  const entry = EVENT_VOCABULARY.find(candidate => candidate.event === event)
  if (entry === undefined) throw new RangeError(`unknown physical event: ${event}`)
  const differs = entry.differs[backend]
  return differs === undefined ? entry.names : Object.freeze({ ...entry.names, ...differs.names })
}

function cell(value: string | null): string {
  return value === null ? '—' : `\`${value}\``
}

/** The Markdown table `docs/UNIFIED_SEMANTICS.md` embeds. */
export function renderEventVocabularyTable(): string {
  const header = [
    '| Physical event | Error code | Retryability | Lifecycle `current` / `cause` | Stream terminal | Supervisor (context → decision) | Differs |',
    '| --- | --- | --- | --- | --- | --- | --- |'
  ]
  const rows = EVENT_VOCABULARY.map(entry => {
    const lifecycle =
      entry.names.lifecycle === null ? '—' : `\`${entry.names.lifecycle.current}\` / \`${entry.names.lifecycle.cause}\``
    const differs = Object.entries(entry.differs)
      .map(([backend, exception]) => {
        const replaced = Object.entries(exception?.names ?? {})
          .map(([field, value]) => `${field} \`${String(value)}\``)
          .join(', ')
        return `${backend}: ${replaced} — ${exception?.why ?? ''}`
      })
      .join('<br>')
    return `| \`${entry.event}\`: ${entry.description} | ${cell(entry.names.error)} | ${cell(entry.names.retryability)} | ${lifecycle} | ${cell(entry.names.streamTerminal)} | ${entry.supervisor.context} → ${entry.supervisor.decision} | ${differs.length === 0 ? 'none' : differs} |`
  })
  return [...header, ...rows].join('\n')
}

/** The table as the JSON the Rust mapping tests read. */
export function eventVocabularyFixture(): string {
  const fixture = Object.fromEntries(
    EVENT_VOCABULARY.map(entry => [
      entry.event,
      Object.fromEntries(VOCABULARY_BACKENDS.map(backend => [backend, eventNamesFor(entry.event, backend)]))
    ])
  )
  return `${JSON.stringify(fixture, null, 2)}\n`
}
