/** Small deterministic Web Locks model, shared by independent module realms. */
function admissionLocks() {
  const held = new Map()
  return {
    request(name, options, callback) {
      const existing = held.get(name) || []
      if (existing.length && (options.mode === 'exclusive' || existing.some(entry => entry.mode === 'exclusive')))
        return Promise.resolve(callback(null))
      const entry = { name, mode: options.mode }
      held.set(name, [...existing, entry])
      return Promise.resolve()
        .then(() => callback(entry))
        .finally(() => {
          const remaining = (held.get(name) || []).filter(value => value !== entry)
          if (remaining.length) held.set(name, remaining)
          else held.delete(name)
        })
    },
    async query() {
      return { held: [...held.values()].flat(), pending: [] }
    }
  }
}
module.exports = { admissionLocks }

const wrappers = new WeakMap()
/** Add admission coordination without changing the existing clear-lock fake. */
function withAdmissionLocks(base) {
  if (!wrappers.has(base)) {
    const admission = admissionLocks()
    wrappers.set(base, {
      request(name, options, callback) {
        return name.includes(':clear-admission-v1:')
          ? admission.request(name, options, callback)
          : base.request(name, options, callback)
      },
      query: () => admission.query()
    })
  }
  return wrappers.get(base)
}
module.exports.withAdmissionLocks = withAdmissionLocks
