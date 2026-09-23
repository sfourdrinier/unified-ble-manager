import { IPC_PROTOCOL_VERSION } from '../ipc/protocol'

export const TAURI_PLUGIN_COMPATIBILITY = Object.freeze({
  npmRange: '^5.0.0-rc.3',
  crateRange: '^5.0.0-rc.3',
  ipcProtocol: IPC_PROTOCOL_VERSION,
  // F01: the linked ubm-core contract revision the candidate plugin must
  // report at bootstrap. Pinned like the ranges above; the Tauri suite
  // asserts it still equals the frozen contracts revision.
  contractRevision: 'C-UBM.0.1.2-DRAFT'
})
