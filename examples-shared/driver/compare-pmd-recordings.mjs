#!/usr/bin/env node
import { open } from 'node:fs/promises'
import { summarizePmdRecording } from './pmd-recording-analysis.ts'

const files = process.argv.slice(2)
if (files.length !== 2) {
  console.error('Usage: node examples-shared/driver/compare-pmd-recordings.mjs simulator.json real-h10.json')
  process.exitCode = 1
} else {
  try {
    const captures = []
    for (const file of files) {
      const handle = await open(file, 'r')
      try {
        if ((await handle.stat()).size > 12 * 1024 * 1024)
          throw new Error(`${file}: recording exceeds 12 MiB input limit`)
        captures.push({ file, ...summarizePmdRecording(JSON.parse(await handle.readFile('utf8'))) })
      } finally {
        await handle.close()
      }
    }
    console.log(
      JSON.stringify(
        {
          captures,
          note: 'Compare matching settings and generations. Synthetic waveforms, different motion and host clocks are not expected to match; these statistics do not establish device equivalence.'
        },
        null,
        2
      )
    )
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error))
    process.exitCode = 1
  }
}
