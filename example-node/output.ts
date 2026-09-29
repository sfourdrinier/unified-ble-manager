import type { Writable } from 'node:stream'

type OutputStream = Pick<Writable, 'write' | 'once' | 'removeListener'>

/** Wait for prior writes, not for native handles to disappear. A stalled consumer
 * cannot indefinitely block shutdown; timeout and stream errors remain failures. */
export async function flushDriverOutput(stdout: OutputStream, stderr: OutputStream, timeoutMs = 5000): Promise<void> {
  let timer: ReturnType<typeof setTimeout> | undefined
  const flush = (stream: OutputStream) =>
    new Promise<void>((resolve, reject) => {
      const onError = (error: Error) => reject(error)
      stream.once('error', onError)
      try {
        stream.write('', error => {
          if (error != null) {
            // Writable emits the same error after its callback. Keep the
            // once-listener until that event, even if the deadline already won.
            reject(error)
          } else {
            stream.removeListener('error', onError)
            resolve()
          }
        })
      } catch (error) {
        stream.removeListener('error', onError)
        reject(error)
      }
    })
  try {
    await Promise.race([
      Promise.all([flush(stdout), flush(stderr)]),
      new Promise<never>((_resolve, reject) => {
        timer = setTimeout(() => reject(new Error('driver output did not drain before shutdown deadline')), timeoutMs)
      })
    ])
  } finally {
    clearTimeout(timer)
  }
}
