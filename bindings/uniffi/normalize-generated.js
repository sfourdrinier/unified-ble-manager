#!/usr/bin/env node
// UniFFI 0.32.1 emits trailing spaces in otherwise valid source lines.
// Keep the checked-in recipe and its reproducibility check byte-identical.
const fs = require('node:fs')
const path = require('node:path')

const directory = process.argv[2]
if (!directory || process.argv.length !== 3) {
  throw new Error('usage: node normalize-generated.js <generated-directory>')
}

for (const source of [
  'kotlin/uniffi/ubm_echo/ubm_echo.kt',
  'swift/ubm_echo.swift',
  'swift/ubm_echoFFI.h',
  'python/ubm_echo.py'
]) {
  const file = path.join(directory, source)
  const original = fs.readFileSync(file, 'utf8')
  const normalized = original.replace(/[ \t]+(?=\r?$)/gm, '')
  if (normalized !== original) fs.writeFileSync(file, normalized)
}
