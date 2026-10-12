import type { IDockviewPanelProps } from 'dockview-react';

/**
 * Placeholder panels from brief §4; later slices replace the content
 * (P4 session timeline, P6 diff). The terminal panel moved to its own
 * module in P3a.
 */

export function TimelinePanel(_props: IDockviewPanelProps) {
  return (
    <div className="panel-placeholder">
      <h2>Timeline</h2>
      <p>wkrótce — tail sesji JSONL</p>
    </div>
  );
}

export function DiffPanel(_props: IDockviewPanelProps) {
  return (
    <div className="panel-placeholder">
      <h2>Diff</h2>
      <p>wkrótce — liniowy + semantyczny</p>
    </div>
  );
}
