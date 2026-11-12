// src/core/ordered-stream-origin.ts
/** Preserve host-owned recovery identity through public and portable stream views. */
const origins = new WeakMap<object, object>()
export function registerOrderedStreamOrigin(view: object, source: object): void {
  origins.set(view, source)
}
export function orderedStreamOrigin(view: object): object {
  let current = view
  for (;;) {
    const source = origins.get(current)
    if (source === undefined) return current
    current = source
  }
}
