'use strict'

// `DispatchRadio` (bindings/napi/src/dispatch.rs) wraps the production and the
// synthetic radio. A `RadioBoundary` method with a default body that the
// wrapper does not forward silently answers the trait default for both
// radios. That has happened twice: `admission_policy` (findings 57/58) and
// `os_answers_unflagged_subscribe` (finding 98). This guard reads the trait
// and the wrapper, and fails when a method is not forwarded to both inner
// radios.

const fs = require('node:fs')
const path = require('node:path')

const ROOT = path.join(__dirname, '..', '..', '..')
const BOUNDARY = path.join(ROOT, 'crates', 'ubm-desktop', 'src', 'boundary.rs')
const DISPATCH = path.join(ROOT, 'bindings', 'napi', 'src', 'dispatch.rs')

function blockAfter(source, header) {
  const start = source.indexOf(header)
  if (start < 0) throw new Error(`missing ${header}`)
  const open = source.indexOf('{', start)
  let depth = 0
  for (let index = open; index < source.length; index += 1) {
    if (source[index] === '{') depth += 1
    if (source[index] === '}') {
      depth -= 1
      if (depth === 0) return source.slice(open + 1, index)
    }
  }
  throw new Error(`unterminated ${header}`)
}

/** Top-level `fn` items of a block, with their body (or null for a required method). */
function functions(block) {
  const items = []
  let depth = 0
  for (let index = 0; index < block.length; index += 1) {
    const character = block[index]
    if (character === '{') depth += 1
    if (character === '}') depth -= 1
    if (depth !== 0) continue
    const match = /^fn\s+(\w+)/u.exec(block.slice(index))
    if (match === null || /\w/u.test(block[index - 1] ?? ' ')) continue
    const terminator = block.slice(index).search(/[;{]/u)
    const isRequired = block[index + terminator] === ';'
    let body = null
    if (!isRequired) {
      const open = index + terminator
      let bodyDepth = 0
      for (let end = open; end < block.length; end += 1) {
        if (block[end] === '{') bodyDepth += 1
        if (block[end] === '}') {
          bodyDepth -= 1
          if (bodyDepth === 0) {
            body = block.slice(open, end + 1)
            index = end
            break
          }
        }
      }
    }
    items.push({ name: match[1], required: isRequired, body })
  }
  return items
}

describe('DispatchRadio forwards every RadioBoundary method', () => {
  const trait = functions(blockAfter(fs.readFileSync(BOUNDARY, 'utf8'), 'pub trait RadioBoundary'))
  const wrapper = new Map(
    functions(blockAfter(fs.readFileSync(DISPATCH, 'utf8'), 'impl RadioBoundary for DispatchRadio')).map(item => [
      item.name,
      item
    ])
  )

  test('the trait scan sees the defaulted methods that were missed before', () => {
    const defaulted = trait.filter(item => !item.required).map(item => item.name)
    expect(defaulted).toEqual(expect.arrayContaining(['admission_policy', 'os_answers_unflagged_subscribe']))
  })

  test.each(trait.map(item => [item.name]))('%s is forwarded to both inner radios', name => {
    const forwarded = wrapper.get(name)
    expect(forwarded).toBeDefined()
    for (const variant of ['Radio', 'Synthetic']) {
      expect(forwarded.body).toMatch(
        new RegExp(`Self::${variant}\\(radio\\) =>\\s*\\{?\\s*radio\\s*\\.${name}\\(`, 'u')
      )
    }
  })
})
