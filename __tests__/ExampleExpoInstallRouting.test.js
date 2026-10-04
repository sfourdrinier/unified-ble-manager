'use strict'

jest.mock('node:child_process', () => ({ spawnSync: jest.fn() }))

const { spawnSync } = require('node:child_process')
const { fixExampleExpoDependencies } = require('../examples-shared/dev/install-example-dependencies')

beforeEach(() => {
  spawnSync.mockReset()
  spawnSync.mockReturnValue({ status: 0 })
})

test('Expo SDK alignment forwards independent workspace admission into every underlying install', () => {
  fixExampleExpoDependencies('example-expo')
  expect(spawnSync).toHaveBeenCalledTimes(1)
  expect(spawnSync.mock.calls[0][1]).toEqual([
    '--dir',
    'example-expo',
    'exec',
    'expo',
    'install',
    '--fix',
    '--',
    '--ignore-workspace'
  ])
})

test('Expo alignment preserves command failure rather than reporting success', () => {
  spawnSync.mockReturnValue({ status: 3 })
  expect(() => fixExampleExpoDependencies('example-expo')).toThrow('failed (exit 3)')
})
