/** Capacity phase survives Stop, so the recorder's accepting fact—not phase
 * alone—owns whether Stop/Clear are available. */
export function recordingControls(phase: string, accepting: boolean, exporting: boolean) {
  const exportable = phase === 'stopped' || phase === 'capacity-reached'
  return {
    record: phase === 'empty' && !accepting && !exporting,
    stop: accepting && (phase === 'recording' || phase === 'capacity-reached'),
    clear: exportable && !accepting && !exporting,
    export: exportable && !exporting
  }
}
