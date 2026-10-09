// __tests__/helpers/api-reports/api-report-symbol-noise.ts
declare const noiseOne: unique symbol
declare const noiseTwo: unique symbol

export type SymbolNoise = {
  [noiseOne]: 'noise-one'
  [noiseTwo]: 'noise-two'
}
