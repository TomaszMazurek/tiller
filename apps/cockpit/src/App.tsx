import { useCallback, useMemo, useState } from 'react';
import { DockviewReact, type DockviewReadyEvent, type IDockviewPanelProps } from 'dockview-react';
import { StatusBar } from './components/StatusBar';
import { ReviewPanel } from './panels/ReviewPanel';
import { TerminalPanel } from './panels/TerminalPanel';
import { TimelinePanel } from './panels/TimelinePanel';
import {
  TerminalRequestContext,
  type TerminalStartRequest,
} from './lib/terminalRequest';
import { ActiveSessionContext } from './lib/activeSession';
import type { SessionId } from './lib/termApi';

/**
 * The terminal session the timeline observes. Lifted here (TerminalPanel
 * reports it up over a stable callback) and published to the timeline
 * through `ActiveSessionContext` (P8a-2): dockview freezes the component
 * function per panel, so a variable id threaded as a panel prop would
 * never reach the timeline mounted at startup.
 *
 * The pending terminal request (P8a) travels the other way, through
 * `TerminalRequestContext` (see that module): ReviewPanel *asks*
 * (requestTerminal), App stores it, TerminalPanel *consumes* it. Both
 * channels are contexts — not panel props — for the same reason: variable
 * data reaching mounted panels must flow through the provider subtree,
 * not the `components` factories.
 */
export function App() {
  const [activeSessionId, setActiveSessionId] = useState<SessionId | null>(null);
  const activeSession = useMemo(() => ({ sessionId: activeSessionId }), [activeSessionId]);
  const [terminalRequest, setTerminalRequest] = useState<TerminalStartRequest | null>(null);
  const requestTerminal = useCallback((program: string, cwd: string) => {
    setTerminalRequest({ program, cwd });
  }, []);
  const consumeTerminalRequest = useCallback(() => setTerminalRequest(null), []);
  const terminalRequestChannel = useMemo(
    () => ({ request: terminalRequest, consume: consumeTerminalRequest }),
    [terminalRequest, consumeTerminalRequest],
  );

  // dockview owns panel instantiation and freezes this map at mount, so
  // only stable seams (callbacks whose identity never changes) belong
  // here; variable data rides the context providers below.
  const components = useMemo(
    () => ({
      terminal: (props: IDockviewPanelProps) => (
        <TerminalPanel {...props} onActiveSessionChange={setActiveSessionId} />
      ),
      timeline: (props: IDockviewPanelProps) => <TimelinePanel {...props} />,
      review: (props: IDockviewPanelProps) => (
        <ReviewPanel {...props} requestTerminal={requestTerminal} />
      ),
    }),
    [requestTerminal],
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
      id: 'review',
      component: 'review',
      title: 'Review',
      position: { referencePanel: 'timeline', direction: 'below' },
    });
  }, []);

  return (
    <div className="app-shell">
      <div className="app-shell__dock dockview-theme-abyss">
        <ActiveSessionContext.Provider value={activeSession}>
          <TerminalRequestContext.Provider value={terminalRequestChannel}>
            <DockviewReact components={components} onReady={onReady} />
          </TerminalRequestContext.Provider>
        </ActiveSessionContext.Provider>
      </div>
      <StatusBar />
    </div>
  );
}
