import { useCallback, useMemo, useState } from 'react';
import { DockviewReact, type DockviewReadyEvent, type IDockviewPanelProps } from 'dockview-react';
import { StatusBar } from './components/StatusBar';
import { DiffPanel } from './panels/PlaceholderPanels';
import { TerminalPanel } from './panels/TerminalPanel';
import { TimelinePanel } from './panels/TimelinePanel';
import type { SessionId } from './lib/termApi';

/**
 * The terminal session the timeline observes. Lifted here (TerminalPanel
 * reports it up, TimelinePanel receives it down) — the minimal shared
 * state between the two panels, no context needed for one value.
 */
export function App() {
  const [activeSessionId, setActiveSessionId] = useState<SessionId | null>(null);

  // dockview owns panel instantiation, so the registry closes over the
  // active-session state; it rebuilds only when that state changes.
  const components = useMemo(
    () => ({
      terminal: (props: IDockviewPanelProps) => (
        <TerminalPanel {...props} onActiveSessionChange={setActiveSessionId} />
      ),
      timeline: (props: IDockviewPanelProps) => (
        <TimelinePanel {...props} activeSessionId={activeSessionId} />
      ),
      diff: DiffPanel,
    }),
    [activeSessionId],
  );

  const onReady = useCallback((event: DockviewReadyEvent) => {
    const api = event.api;
    api.addPanel({ id: 'terminal', component: 'terminal', title: 'Terminal' });
    api.addPanel({
      id: 'timeline',
      component: 'timeline',
      title: 'Timeline',
      position: { referencePanel: 'terminal', direction: 'right' },
    });
    api.addPanel({
      id: 'diff',
      component: 'diff',
      title: 'Diff',
      position: { referencePanel: 'timeline', direction: 'below' },
    });
  }, []);

  return (
    <div className="app-shell">
      <div className="app-shell__dock dockview-theme-abyss">
        <DockviewReact components={components} onReady={onReady} />
      </div>
      <StatusBar />
    </div>
  );
}
