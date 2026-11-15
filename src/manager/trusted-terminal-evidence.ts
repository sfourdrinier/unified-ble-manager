/**
 * An Electron-main-only capability carried outside public operation options.
 * The WeakMap prevents renderer-controlled records from manufacturing a
 * source binding: only the trusted router can bind the exact options object
 * after it has authenticated the reservation owner and producer.
 */
export interface TrustedTerminalEvidenceScope {
  readonly managerId: string
  readonly operationId: string
  readonly incarnation: string
}

const scopes = new WeakMap<object, TrustedTerminalEvidenceScope>()

export function bindTrustedTerminalEvidenceScope(target: object, scope: TrustedTerminalEvidenceScope): void {
  if (scopes.has(target)) throw new Error('A trusted terminal-evidence scope is already bound to this operation.')
  scopes.set(target, scope)
}

export function takeTrustedTerminalEvidenceScope(target: object): TrustedTerminalEvidenceScope | null {
  const scope = scopes.get(target) ?? null
  scopes.delete(target)
  return scope
}
