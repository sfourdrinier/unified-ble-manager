'use strict'

const { driveVirtualClock } = require('../helpers/async')

test('virtual driver follows completion beyond an arbitrary twenty-turn limit', async () => {
  let complete
  const pending = new Promise(resolve => {
    complete = resolve
  })
  let turns = 0
  const clock = {
    runUntilIdle() {
      if (++turns === 30) complete('completed')
    }
  }
  await expect(driveVirtualClock(clock, pending)).resolves.toBe('completed')
  expect(turns).toBe(30)
})

test('virtual driver preserves the original operation rejection', async () => {
  const error = new Error('original refusal')
  let refuse
  const pending = new Promise((_resolve, reject) => {
    refuse = reject
  })
  const clock = {
    runUntilIdle() {
      refuse(error)
    }
  }
  await expect(driveVirtualClock(clock, pending)).rejects.toBe(error)
})

test('virtual driver reports scheduler failure rather than abandoning its wait', async () => {
  const error = new Error('scheduler failure')
  const clock = {
    runUntilIdle() {
      throw error
    }
  }
  await expect(driveVirtualClock(clock, new Promise(() => {}))).rejects.toBe(error)
})
