#!/usr/bin/env node
'use strict'

const { NATIVE_PREBUILD_TARGETS } = require('./targets')

process.stdout.write(
  JSON.stringify({
    include: NATIVE_PREBUILD_TARGETS.map(({ backend, platform, arch, runner, artifactName, builder, rustTarget }) => ({
      backend,
      platform,
      arch,
      runner,
      artifactName,
      builder,
      rustTarget: rustTarget ?? ''
    }))
  })
)
