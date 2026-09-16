// contracts/src/index.ts — C-UBM DRAFT central API surface (pending U1 acceptance).
// The single entrypoint for the frozen contract draft. No imports from
// src/**, no private app-model content, no RxJS.

export { freezeTable } from './freeze';

export {
  BUILD_VERSION_IS_HANDSHAKE_AXIS,
  CONTRACT_ACCEPTANCE_GATE,
  CONTRACT_REVISION,
  CONTRACT_STATUS,
  RUNTIME_AXES,
  assertContractRevisionEqual,
  assertHandshakeComplete,
  assertNegotiatedWithinOffer,
  isRuntimeAxis,
  makeVersionSpan,
  negotiateCoreOffer,
  negotiateIpcOffer,
  negotiateNativeOffer,
  negotiateVersionSpan,
} from './version';
export type {
  CoreHandshakeOffer,
  CoreNegotiated,
  HandshakeState,
  IpcHandshakeOffer,
  IpcNegotiated,
  NativeHandshakeOffer,
  NativeNegotiated,
  NegotiatedAxis,
  RuntimeAxis,
  VersionSpan,
} from './version';

export {
  PEER_IDENTITY_DOMAINS,
  assertCurrentGeneration,
  assertSameAttachment,
  attachmentTuplesEqual,
  canonicalBleAddressValue,
  canonicalUuidValue,
  createAttachmentTuple,
  createGattPath,
  createHandleRef,
  createPeerIdentity,
  isGenerationCurrent,
  isGloballyStableDomain,
  isNonEmptyId,
  peerSessionKey,
} from './identities';
export type {
  AttachmentTuple,
  GattPath,
  HandleRef,
  PeerIdentity,
  PeerIdentityDomain,
} from './identities';

export {
  ContractError,
  ERROR_CODE_LIST,
  ERROR_DOMAIN_LIST,
  OPERATION_TERMINAL_KINDS,
  PROFILE_CODEC_ERROR_CODES,
  contractError,
  isBleErrorCode,
  isBleErrorDomain,
  isContractError,
  isProfileCodecErrorCode,
  makePlatformDetail,
  makeTerminalRecord,
  recoveryFor,
} from './outcomes';
export type {
  BleErrorCode,
  BleErrorDomain,
  OperationTerminalKind,
  PlatformDetail,
  ProfileCodecErrorCode,
  Recoverability,
  Recovery,
  RecoveryActionKind,
  RecoveryDisposition,
  TerminalRecord,
} from './outcomes';

export {
  ADAPTER_OWNER_AGGREGATE_BYTES,
  BACKEND_INGRESS_AGGREGATE_BYTES,
  CLIENT_AGGREGATE_BYTES,
  I64_MAX,
  I64_MIN,
  MAX_DECIMAL_DIGITS,
  MAX_IPC_LEASES_PER_IDENTITY,
  MAX_OPERATION_BYTES,
  MAX_SCAN_STATE_BYTES,
  MAX_SCAN_STATE_ENTRIES,
  MAX_STREAM_BYTE_CAPACITY,
  MAX_STREAM_ITEM_CAPACITY,
  MAX_TIMEOUT_MS,
  MIN_STREAM_ITEM_CAPACITY,
  MIN_TIMEOUT_MS,
  TRACE_MAX_BYTES,
  U64_MAX,
  U64_MIN,
  assertByteCapacity,
  assertBytesWithinLimit,
  assertItemCapacity,
  assertTimeoutMs,
  earliestDeadline,
  effectiveMaxBytes,
  isDeadlineExpired,
  isMonotonicNow,
  isSafeJsInteger,
  parseI64Decimal,
  parseU64Decimal,
  toDeadline,
} from './bounds';

export {
  BUILT_IN_CAPABILITY_IDS,
  CAPABILITY_STATES,
  assertCapabilityAllows,
  isBuiltInCapabilityId,
  makeCapabilityDescriptor,
} from './capabilities';
export type {
  CapabilityAdmission,
  CapabilityDescriptor,
  CapabilityState,
  EvidenceLevel,
  EvidenceReceipt,
  Limitation,
} from './capabilities';

export {
  arbitrateConnectionRequest,
  arbitrateScanRequest,
  isAuthorizationBlocking,
  validateOwnershipTransfer,
} from './hosts';
export type {
  AdapterAuthorization,
  AdapterAvailability,
  AdapterPower,
  AdapterSnapshot,
  HostKind,
  ManagerMode,
  OwnershipDecision,
  OwnershipTransfer,
} from './hosts';

export {
  HAPPENS_BEFORE,
  arbitrateContenders,
  assertHandshakeBeforeEffects,
  makeEffect,
} from './effects';
export type {
  CommitState,
  CompletionRecord,
  CompletionTerminal,
  Contender,
  ContenderKind,
  Effect,
  EffectKind,
  HappensBeforePair,
} from './effects';

export {
  CENTRAL_CONTROLS,
  planLongWrite,
  validateScanRequest,
  validateWriteRequest,
} from './central';
export type {
  CentralControl,
  LongWritePlan,
  ScanDuplicatePolicy,
  ScanMergePolicy,
  ScanRequest,
  WriteMode,
  WriteRequest,
} from './central';

export {
  GENERIC_PERIPHERAL_ALLOWED_KEYS,
  arbitrateServerResponse,
  assertAdvertisementWithinLimits,
  assertAtomicCommit,
  assertGenericPeripheralDecl,
  reconcileCccdOnUnsubscribe,
  validateRoleConcurrency,
  validateServiceDecl,
  validateTargetedNotification,
} from './peripheral';
export type {
  CccdBinding,
  CccdReconciliation,
  CharacteristicDecl,
  DescriptorDecl,
  GattProperty,
  RoleConcurrency,
  RoleKind,
  ServerResponseVerdict,
  ServiceDecl,
  TargetedNotification,
} from './peripheral';

export {
  EARLY_EXIT_CLEANUP,
  combineCleanupRecords,
  createCounterLedger,
  makeCleanupRecord,
} from './cleanup';
export type {
  CleanupFailure,
  CleanupRecord,
  CleanupState,
  CounterLedger,
  EarlyExitRow,
  ResourceCounterKind,
} from './cleanup';

export {
  OVERFLOW_POLICIES,
  RESERVED_CONTROL_BYTES,
  RESERVED_CONTROL_CAPACITY,
  STREAM_DEFAULTS,
  applyStreamAdmission,
  validateStreamLimits,
} from './streams';
export type {
  AdmissionDecision,
  AdmissionResult,
  OverflowPolicy,
  StreamAccounting,
  StreamDefault,
  StreamLimits,
  StreamName,
} from './streams';

export {
  CONTENTION_RULINGS,
  TRANSITION_TABLES,
  isTerminalState,
  isTransitionAllowed,
} from './transitions';
export type {
  ContentionRuling,
  MachineName,
  MachineTable,
  TransitionRow,
} from './transitions';

export {
  APPROVED_CORRECTIONS,
  MANDATORY_SCENARIOS,
  SEMANTIC_MAP,
  correctionForMapping,
} from './semantic-map';
export type {
  ApprovedCorrection,
  MapDisposition,
  SemanticMapping,
} from './semantic-map';

export { INVALID_FIXTURES } from './fixtures/invalid';
export type { InvalidFixture } from './fixtures/invalid';
export { VALID_FIXTURES } from './fixtures/valid';
export type { ValidFixture } from './fixtures/valid';
