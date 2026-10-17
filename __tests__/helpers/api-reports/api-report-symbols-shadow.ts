// __tests__/helpers/api-reports/api-report-symbols-shadow.ts
export declare const custom: unique symbol
export declare const Symbol: { token: typeof custom }
export interface ShadowSurface {
  [Symbol.token]: string
}
