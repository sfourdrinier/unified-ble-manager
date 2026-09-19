// examples-shared/driver/browser/panel.ts
//
// The scenario screen of every browser-engine host (Web, Tauri, Electron
// renderer): the same registry the control server drives, rendered as preset
// buttons, headline, snapshot and recent events, plus the remote-channel
// status and any pending user-gesture request as a real button.

import type { JsonValue, ScenarioEvent } from '../protocol.ts'
import { describeError } from '../protocol.ts'
import type { RemoteDriverChannel, RemoteDriverState } from '../remote-channel.ts'
import type { Scenario, ScenarioRegistry } from '../scenario-core.ts'
import type { PendingUserGestureGate, UserGestureRequest } from '../user-gesture.ts'

const RECENT_EVENTS_SHOWN = 12

export interface PanelOptions {
  readonly mount: HTMLElement
  readonly title: string
  readonly registry: ScenarioRegistry
  readonly remote: RemoteDriverChannel
  readonly userGesture: PendingUserGestureGate | null
}

export function mountDriverPanel(options: PanelOptions): () => void {
  const { mount, registry, remote, userGesture } = options
  const document = mount.ownerDocument
  const element = <Tag extends keyof HTMLElementTagNameMap>(tag: Tag, className?: string, text?: string): HTMLElementTagNameMap[Tag] => {
    const node = document.createElement(tag)
    if (className !== undefined) node.className = className
    if (text !== undefined) node.textContent = text
    return node
  }

  mount.replaceChildren()
  mount.append(element('h1', 'driver-title', options.title))
  const remoteLine = element('p', 'driver-remote')
  mount.append(remoteLine)
  const gestures = element('div', 'driver-gestures')
  mount.append(gestures)
  const cleanups: (() => void)[] = []

  const renderRemote = (state: RemoteDriverState) => {
    const parts = [`remote: ${state.status}`, state.url ?? 'no url', state.hostId === null ? null : `id ${state.hostId}`, state.lastError]
    remoteLine.textContent = parts.filter(part => part !== null).join(' · ')
  }
  renderRemote(remote.state())
  cleanups.push(remote.subscribe(renderRemote))

  if (userGesture !== null) {
    const renderGestures = (pending: readonly UserGestureRequest[]) => {
      gestures.replaceChildren(
        ...pending.map(request => {
          const button = element('button', 'driver-gesture', `Open chooser for ${request.scenario}`)
          button.type = 'button'
          button.title = request.reason
          button.addEventListener('click', () => userGesture.grant(request.id))
          return button
        })
      )
    }
    renderGestures(userGesture.pending())
    cleanups.push(userGesture.subscribe(renderGestures))
  }

  for (const scenario of registry.list()) cleanups.push(mountScenario(scenario, mount, element))
  return () => {
    for (const cleanup of cleanups) cleanup()
  }
}

type ElementFactory = <Tag extends keyof HTMLElementTagNameMap>(tag: Tag, className?: string, text?: string) => HTMLElementTagNameMap[Tag]

function mountScenario(scenario: Scenario, mount: HTMLElement, element: ElementFactory): () => void {
  const card = element('section', 'driver-scenario')
  const heading = element('h2', undefined, scenario.title)
  const headline = element('p', 'driver-headline')
  const description = element('p', 'driver-description', scenario.description)
  const buttons = element('div', 'driver-buttons')
  const outcome = element('pre', 'driver-outcome')
  const snapshot = element('pre', 'driver-snapshot')
  const events = element('pre', 'driver-events')
  for (const command of scenario.describe().commands) {
    for (const preset of command.presets) {
      const button = element('button', undefined, preset.label)
      button.type = 'button'
      button.title = command.description
      button.addEventListener('click', () => {
        outcome.textContent = `${command.name} running…`
        scenario.dispatch(command.name, preset.args).then(
          result => (outcome.textContent = `${command.name} → ${pretty(result)}`),
          error => (outcome.textContent = `${command.name} failed → ${pretty(describeError(error))}`)
        )
      })
      buttons.append(button)
    }
  }
  card.append(heading, headline, description, buttons, outcome, snapshot, events)
  mount.append(card)

  let frame: number | null = null
  const render = () => {
    frame = null
    headline.textContent = scenario.headline() ?? ''
    snapshot.textContent = pretty(scenario.snapshot())
    events.textContent = scenario.recentEvents().slice(-RECENT_EVENTS_SHOWN).map(eventLine).join('\n')
  }
  render()
  const unsubscribe = scenario.subscribe(() => {
    if (frame === null) frame = requestAnimationFrame(render)
  })
  return () => {
    unsubscribe()
    if (frame !== null) cancelAnimationFrame(frame)
  }
}

function pretty(value: JsonValue): string {
  return JSON.stringify(value, null, 2)
}

function eventLine(event: ScenarioEvent): string {
  return `#${event.seq.toString()} +${Math.round(event.atMs).toString()}ms ${event.kind} ${JSON.stringify(event.data)}`
}
