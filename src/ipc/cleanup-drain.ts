/** Bounds child cleanup drain without cancelling the underlying retry obligation. */
export const DESTROY_CHILD_DRAIN_BUDGET_MS = 50

export async function waitForChildCleanup(
  work: Promise<unknown>
): Promise<{ readonly error?: unknown; readonly pending?: true }> {
  let timer: ReturnType<typeof setTimeout> | null = null
  try {
    return await Promise.race([
      work.then(
        () => ({}),
        error => ({ error })
      ),
      new Promise<{ readonly pending: true }>(resolve => {
        timer = setTimeout(() => resolve({ pending: true }), DESTROY_CHILD_DRAIN_BUDGET_MS)
      })
    ])
  } finally {
    if (timer !== null) clearTimeout(timer)
  }
}
