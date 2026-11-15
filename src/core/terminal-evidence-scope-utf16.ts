/** Lossless JavaScript UTF-16 scope framing shared by native-host boundaries. */
export type TerminalEvidenceScopeText = Readonly<{
  managerId: string
  operationId: string
  incarnation: string
}>

export function terminalEvidenceScopeUtf16le(
  producer: 'scan' | 'subscription',
  scope: TerminalEvidenceScopeText
): Uint8Array | undefined {
  const fields = [producer, scope.managerId, scope.operationId, scope.incarnation]
  const units = fields.map(utf16Units)
  if (!units.some(hasUnpairedUtf16)) return undefined
  const bytes = new Uint8Array(16 + units.reduce((total, field) => total + field.length * 2, 0))
  const view = new DataView(bytes.buffer)
  let offset = 0
  for (const field of units) {
    view.setUint32(offset, field.length, true)
    offset += 4
  }
  for (const field of units) {
    for (const unit of field) {
      view.setUint16(offset, unit, true)
      offset += 2
    }
  }
  return bytes
}

export function projectTerminalEvidenceScopeUtf16(value: string): string {
  let projected = ''
  for (let index = 0; index < value.length; index += 1) {
    const unit = value.charCodeAt(index)
    if (isHigh(unit) && index + 1 < value.length && isLow(value.charCodeAt(index + 1))) {
      projected += value[index]! + value[index + 1]!
      index += 1
    } else projected += isHigh(unit) || isLow(unit) ? '\ufffd' : value[index]!
  }
  return projected
}

export function unpackTerminalEvidenceScopeUtf16le(
  raw: Uint8Array
): readonly [string, string, string, string] | undefined {
  if (raw.byteLength < 16) return undefined
  const view = new DataView(raw.buffer, raw.byteOffset, raw.byteLength)
  const lengths = [view.getUint32(0, true), view.getUint32(4, true), view.getUint32(8, true), view.getUint32(12, true)]
  if (
    lengths.some(length => length === 0) ||
    16 + lengths.reduce((total, length) => total + length * 2, 0) !== raw.byteLength
  )
    return undefined
  let offset = 16
  const fields = lengths.map(length => {
    let text = ''
    for (let index = 0; index < length; index += 1) {
      text += String.fromCharCode(view.getUint16(offset, true))
      offset += 2
    }
    return text
  })
  const [producer, managerId, operationId, incarnation] = fields
  return producer === undefined || managerId === undefined || operationId === undefined || incarnation === undefined
    ? undefined
    : [producer, managerId, operationId, incarnation]
}

export function hasUnpairedUtf16(units: readonly number[]): boolean {
  for (let index = 0; index < units.length; index += 1) {
    const unit = units[index]!
    if (isHigh(unit)) {
      if (!isLow(units[index + 1] ?? -1)) return true
      index += 1
    } else if (isLow(unit)) return true
  }
  return false
}

function utf16Units(value: string): number[] {
  const units: number[] = []
  for (let index = 0; index < value.length; index += 1) units.push(value.charCodeAt(index))
  return units
}
function isHigh(unit: number): boolean {
  return unit >= 0xd800 && unit <= 0xdbff
}
function isLow(unit: number): boolean {
  return unit >= 0xdc00 && unit <= 0xdfff
}
