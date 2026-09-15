// contracts/src/freeze.ts — C-UBM DRAFT shared deep-freeze helper.
//
// Every shared contract table is frozen at declaration so the "frozen"
// contract is runtime-immutable, not merely compile-time `readonly`.
// `freezeTable` deep-freezes arrays, plain records, and their nested
// contents; constructed per-call records keep their own `Object.freeze`
// at the construction site.

export function freezeTable<T>(table: T): T {
  deepFreeze(table);
  return table;
}

function deepFreeze(value: unknown): void {
  if (typeof value !== 'object' || value === null) {
    return;
  }
  if (Object.isFrozen(value)) {
    return;
  }
  if (Array.isArray(value)) {
    for (const entry of value) {
      deepFreeze(entry);
    }
  } else {
    for (const [, entryValue] of Object.entries(value)) {
      deepFreeze(entryValue);
    }
  }
  Object.freeze(value);
}
