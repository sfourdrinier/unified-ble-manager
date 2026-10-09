// __tests__/fixtures/api-report-symbols.ts
declare const firstCustom: unique symbol
declare const secondCustom: unique symbol

declare const firstUnique: unique symbol
const key: typeof firstUnique = firstUnique

export interface ReboundKeySurface {
  [key]: 'rebound-first'
}

export type BacktickSurface = `${string}`

export interface SymbolSurface {
  [Symbol.asyncIterator](): AsyncIterator<string>
  [Symbol.toStringTag]: string
  [firstCustom]: 'first'
  [secondCustom]: 'second'
}
