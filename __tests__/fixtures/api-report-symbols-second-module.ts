// __tests__/fixtures/api-report-symbols-second-module.ts
declare const firstUnique: unique symbol
const key: typeof firstUnique = firstUnique

export interface ReboundKeySurface {
  [key]: 'rebound-first'
}
