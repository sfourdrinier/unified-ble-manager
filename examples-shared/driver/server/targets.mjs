// examples-shared/driver/server/targets.mjs
//
// One targeting rule for the hub, the CLI and sequences: `all`, an exact host
// id, a host kind (expo | web | tauri | electron | node | peripheral-sim) or
// a platform (android | ios | macos | windows | linux), or a list of those
// (any match).

export function matchesTarget(host, target) {
  if (Array.isArray(target)) return target.some(entry => matchesTarget(host, entry))
  if (target === undefined || target === 'all') return true
  return host.hostId === target || host.host === target || host.platform === target
}
