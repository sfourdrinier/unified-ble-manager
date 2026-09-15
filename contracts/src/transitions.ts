// contracts/src/transitions.ts — C-UBM DRAFT (pending U1 acceptance).
//
// Lifecycle state machines, contention rulings, and race winners. Derived
// from docs/UNIFIED_SEMANTICS.md §3/§4/§14 (read-only reference). A stale
// object is never revived; recovery creates a new generation.

export type MachineName =
  | 'provider'
  | 'backend'
  | 'manager'
  | 'operation'
  | 'scan-session'
  | 'chooser-session'
  | 'connection'
  | 'database'
  | 'subscription';

export interface TransitionRow {
  readonly from: string;
  readonly to: string;
  readonly via: string;
}

export interface MachineTable {
  readonly machine: MachineName;
  readonly states: readonly string[];
  readonly transitions: readonly TransitionRow[];
  readonly terminals: readonly string[];
}

export const TRANSITION_TABLES: readonly MachineTable[] = [
  {
    machine: 'provider',
    states: ['ready', 'closing', 'closed'],
    transitions: [
      { from: 'ready', to: 'closing', via: 'close' },
      { from: 'closing', to: 'closed', via: 'children-settled' },
      { from: 'ready', to: 'closing', via: 'child-creation-failure' },
    ],
    terminals: ['closed'],
  },
  {
    machine: 'backend',
    states: ['created', 'negotiating', 'ready', 'resetting', 'stopping', 'stopped', 'failed'],
    transitions: [
      { from: 'created', to: 'negotiating', via: 'adopt' },
      { from: 'negotiating', to: 'ready', via: 'handshake-accepted' },
      { from: 'negotiating', to: 'failed', via: 'handshake-rejected' },
      { from: 'ready', to: 'resetting', via: 'reset' },
      { from: 'resetting', to: 'ready', via: 'renegotiated' },
      { from: 'resetting', to: 'failed', via: 'reset-failed' },
      { from: 'created', to: 'stopping', via: 'stop' },
      { from: 'negotiating', to: 'stopping', via: 'stop' },
      { from: 'ready', to: 'stopping', via: 'stop' },
      { from: 'resetting', to: 'stopping', via: 'stop' },
      { from: 'failed', to: 'stopping', via: 'stop' },
      { from: 'stopping', to: 'stopped', via: 'children-released' },
      { from: 'failed', to: 'stopping', via: 'cleanup' },
    ],
    terminals: ['stopped', 'failed'],
  },
  {
    machine: 'manager',
    states: ['created', 'negotiating', 'ready', 'destroying', 'destroyed'],
    transitions: [
      { from: 'created', to: 'negotiating', via: 'adopt' },
      { from: 'negotiating', to: 'ready', via: 'handshake-accepted' },
      { from: 'created', to: 'destroying', via: 'destroy' },
      { from: 'negotiating', to: 'destroying', via: 'destroy' },
      { from: 'ready', to: 'destroying', via: 'destroy' },
      { from: 'destroying', to: 'destroyed', via: 'children-released' },
    ],
    terminals: ['destroyed'],
  },
  {
    machine: 'operation',
    states: [
      'created',
      'queued',
      'dispatched',
      'settling',
      'succeeded',
      'failed',
      'aborted',
      'timed-out',
      'disconnected',
      'reset',
      'adapter-unavailable',
      'destroyed',
    ],
    transitions: [
      { from: 'created', to: 'queued', via: 'admit' },
      { from: 'queued', to: 'dispatched', via: 'dispatch' },
      { from: 'dispatched', to: 'settling', via: 'contender-won' },
      { from: 'settling', to: 'succeeded', via: 'publish-success' },
      { from: 'settling', to: 'failed', via: 'publish-failure' },
      { from: 'settling', to: 'aborted', via: 'publish-abort' },
      { from: 'settling', to: 'timed-out', via: 'publish-timeout' },
      { from: 'settling', to: 'disconnected', via: 'publish-disconnect' },
      { from: 'settling', to: 'reset', via: 'publish-reset' },
      { from: 'settling', to: 'adapter-unavailable', via: 'publish-adapter-loss' },
      { from: 'settling', to: 'destroyed', via: 'publish-destroy' },
      { from: 'created', to: 'aborted', via: 'pre-abort' },
      { from: 'queued', to: 'aborted', via: 'queued-abort' },
      { from: 'queued', to: 'timed-out', via: 'queued-deadline' },
      { from: 'queued', to: 'disconnected', via: 'queued-disconnect' },
      { from: 'queued', to: 'reset', via: 'queued-reset' },
      { from: 'queued', to: 'destroyed', via: 'queued-destroy' },
      { from: 'queued', to: 'adapter-unavailable', via: 'queued-adapter-loss' },
    ],
    terminals: [
      'succeeded',
      'failed',
      'aborted',
      'timed-out',
      'disconnected',
      'reset',
      'adapter-unavailable',
      'destroyed',
    ],
  },
  {
    machine: 'scan-session',
    states: ['starting', 'active', 'stopping', 'stopped', 'failed'],
    transitions: [
      { from: 'starting', to: 'active', via: 'platform-started' },
      { from: 'starting', to: 'stopping', via: 'stop' },
      { from: 'starting', to: 'failed', via: 'start-failed' },
      { from: 'active', to: 'stopping', via: 'stop' },
      { from: 'active', to: 'stopped', via: 'source-closed' },
      { from: 'active', to: 'failed', via: 'source-failed' },
      { from: 'active', to: 'failed', via: 'overflow-error-policy' },
      { from: 'active', to: 'failed', via: 'reset' },
      { from: 'stopping', to: 'stopped', via: 'platform-stopped' },
      { from: 'stopping', to: 'failed', via: 'stop-failed' },
    ],
    terminals: ['stopped', 'failed'],
  },
  {
    machine: 'chooser-session',
    states: ['requesting', 'selected', 'cancelled', 'failed', 'closed'],
    transitions: [
      { from: 'requesting', to: 'selected', via: 'user-selected' },
      { from: 'requesting', to: 'cancelled', via: 'user-cancelled' },
      { from: 'requesting', to: 'failed', via: 'request-failed' },
      { from: 'requesting', to: 'failed', via: 'deadline' },
      { from: 'selected', to: 'closed', via: 'scope-consumed' },
      { from: 'cancelled', to: 'closed', via: 'settle' },
      { from: 'failed', to: 'closed', via: 'settle' },
    ],
    terminals: ['closed'],
  },
  {
    machine: 'connection',
    states: ['connecting', 'connected', 'disconnecting', 'disconnected', 'lost', 'invalid'],
    transitions: [
      { from: 'connecting', to: 'connected', via: 'link-established' },
      { from: 'connecting', to: 'disconnecting', via: 'disconnect' },
      { from: 'connecting', to: 'lost', via: 'peer-loss' },
      { from: 'connecting', to: 'invalid', via: 'reset' },
      { from: 'connected', to: 'disconnecting', via: 'disconnect' },
      { from: 'connected', to: 'lost', via: 'peer-loss' },
      { from: 'connected', to: 'invalid', via: 'reset' },
      { from: 'disconnecting', to: 'disconnected', via: 'link-released' },
      { from: 'disconnecting', to: 'lost', via: 'peer-loss' },
      { from: 'disconnecting', to: 'invalid', via: 'reset' },
    ],
    terminals: ['disconnected', 'lost', 'invalid'],
  },
  {
    machine: 'database',
    states: ['undiscovered', 'discovering', 'current', 'changed', 'invalid'],
    transitions: [
      { from: 'undiscovered', to: 'discovering', via: 'discover' },
      { from: 'current', to: 'discovering', via: 'rediscover' },
      { from: 'discovering', to: 'current', via: 'snapshot-complete' },
      { from: 'discovering', to: 'undiscovered', via: 'discovery-failed' },
      { from: 'discovering', to: 'invalid', via: 'connection-loss' },
      { from: 'current', to: 'changed', via: 'services-changed' },
      { from: 'current', to: 'invalid', via: 'connection-loss' },
      { from: 'changed', to: 'undiscovered', via: 'require-rediscovery' },
    ],
    terminals: ['invalid'],
  },
  {
    machine: 'subscription',
    states: ['enabling', 'ready', 'removing', 'removed', 'failed', 'invalid'],
    transitions: [
      { from: 'enabling', to: 'ready', via: 'cccd-enabled' },
      { from: 'enabling', to: 'removing', via: 'remove-during-enable' },
      { from: 'enabling', to: 'failed', via: 'enable-failed' },
      { from: 'enabling', to: 'invalid', via: 'connection-loss' },
      { from: 'ready', to: 'removing', via: 'remove' },
      { from: 'ready', to: 'failed', via: 'source-failed' },
      { from: 'ready', to: 'invalid', via: 'connection-loss' },
      { from: 'removing', to: 'removed', via: 'cccd-disabled' },
      { from: 'removing', to: 'invalid', via: 'connection-loss' },
    ],
    terminals: ['removed', 'failed', 'invalid'],
  },
] satisfies readonly MachineTable[];

function tableFor(machine: MachineName): MachineTable | null {
  for (const table of TRANSITION_TABLES) {
    if (table.machine === machine) {
      return table;
    }
  }
  return null;
}

export function isTransitionAllowed(machine: MachineName, from: string, to: string): boolean {
  const table = tableFor(machine);
  if (table === null) {
    return false;
  }
  for (const row of table.transitions) {
    if (row.from === from && row.to === to) {
      return true;
    }
  }
  return false;
}

export function isTerminalState(machine: MachineName, state: string): boolean {
  const table = tableFor(machine);
  if (table === null) {
    return false;
  }
  return table.terminals.some(terminal => terminal === state);
}

export interface ContentionRuling {
  readonly resource: string;
  readonly ruling: string;
  readonly rejection: string | null;
}

export const CONTENTION_RULINGS: readonly ContentionRuling[] = [
  {
    resource: 'ordinary-scan',
    ruling: 'one-physical-scan-controller',
    rejection: 'scan.already-active',
  },
  {
    resource: 'shared-scan',
    ruling: 'independent-bounded-stream-per-share-token',
    rejection: null,
  },
  { resource: 'chooser', ruling: 'per-session-non-shareable', rejection: 'chooser.busy' },
  {
    resource: 'peer-connection-shared',
    ruling: 'independent-lease-per-client-final-release-disconnects',
    rejection: null,
  },
  {
    resource: 'peer-connection-exclusive',
    ruling: 'second-request-fails',
    rejection: 'connection.already-owned',
  },
  {
    resource: 'notification-subscription',
    ruling: 'shared-enablement-per-consumer-streams',
    rejection: null,
  },
] satisfies readonly ContentionRuling[];
