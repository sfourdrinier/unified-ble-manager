/** Serial source-lane fence for terminal evidence staging. */
export class TerminalEvidenceFence {
  private sealed = false
  private tail: Promise<void> = Promise.resolve()
  private failure: unknown = null
  /** Returns false once sealed; callers must not treat that callback as accepted. */
  enqueue(work: () => void | Promise<void>): boolean {
    if (this.sealed) return false
    this.tail = this.tail.then(work, work).catch(error => {
      if (this.failure === null) this.failure = error
    })
    return true
  }
  /** Seals new acceptance and waits for the complete accepted prefix. */
  async seal(): Promise<void> {
    this.sealed = true
    await this.tail
    if (this.failure !== null) throw this.failure
  }
  get isSealed(): boolean {
    return this.sealed
  }
}
