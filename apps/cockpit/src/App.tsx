import { DockviewReact, type DockviewReadyEvent } from 'dockview-react';
import { StatusBar } from './components/StatusBar';
import { DiffPanel, TerminalPanel, TimelinePanel } from './panels/PlaceholderPanels';

const components = {
  terminal: TerminalPanel,
  timeline: TimelinePanel,
  diff: DiffPanel,
};

function onReady(event: DockviewReadyEvent): void {
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
}

export function App() {
  return (
    <div className="app-shell">
      <div className="app-shell__dock dockview-theme-abyss">
        <DockviewReact components={components} onReady={onReady} />
      </div>
      <StatusBar />
    </div>
  );
}
