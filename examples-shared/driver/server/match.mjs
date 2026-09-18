// examples-shared/driver/server/match.mjs
//
// Partial structural matching for sequence expectations. Objects match when
// every expected key matches; `$`-operators compare scalars.

const OPERATORS = {
  $eq: (actual, expected) => Object.is(actual, expected),
  $ne: (actual, expected) => !Object.is(actual, expected),
  $gt: (actual, expected) => typeof actual === 'number' && actual > expected,
  $gte: (actual, expected) => typeof actual === 'number' && actual >= expected,
  $lt: (actual, expected) => typeof actual === 'number' && actual < expected,
  $lte: (actual, expected) => typeof actual === 'number' && actual <= expected,
  $exists: (actual, expected) => (actual !== undefined && actual !== null) === expected,
  $in: (actual, expected) => Array.isArray(expected) && expected.some(candidate => Object.is(candidate, actual)),
  $contains: (actual, expected) =>
    (typeof actual === 'string' && actual.includes(expected)) ||
    (Array.isArray(actual) && actual.some(item => mismatches(item, expected).length === 0)),
  $length: (actual, expected) => (typeof actual === 'string' || Array.isArray(actual)) && mismatches(actual.length, expected).length === 0
}

function isOperatorObject(value) {
  return isPlainObject(value) && Object.keys(value).length > 0 && Object.keys(value).every(key => key.startsWith('$'))
}

function isPlainObject(value) {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

/** Returns human-readable mismatches; an empty list means `actual` satisfies `expected`. */
export function mismatches(actual, expected, path = '$') {
  if (isOperatorObject(expected)) {
    const failures = []
    for (const [operator, operand] of Object.entries(expected)) {
      const check = OPERATORS[operator]
      if (check === undefined) failures.push(`${path}: unknown operator ${operator}`)
      else if (!check(actual, operand)) failures.push(`${path}: expected ${operator} ${JSON.stringify(operand)}, got ${JSON.stringify(actual)}`)
    }
    return failures
  }
  if (Array.isArray(expected)) {
    if (!Array.isArray(actual) || actual.length !== expected.length) {
      return [`${path}: expected array of ${expected.length}, got ${JSON.stringify(actual)}`]
    }
    return expected.flatMap((item, index) => mismatches(actual[index], item, `${path}[${index}]`))
  }
  if (isPlainObject(expected)) {
    if (!isPlainObject(actual)) return [`${path}: expected object, got ${JSON.stringify(actual)}`]
    return Object.entries(expected).flatMap(([key, value]) => mismatches(actual[key], value, `${path}.${key}`))
  }
  return Object.is(actual, expected) ? [] : [`${path}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`]
}
