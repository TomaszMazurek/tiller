import type { IDockviewPanelProps } from 'dockview-react';

/**
 * Placeholder panels from brief §4; later slices replace the content
 * (P6 diff). The terminal panel moved to its own module in P3a, the
 * timeline panel in P4.
 */

export function DiffPanel(_props: IDockviewPanelProps) {
  return (
    <div className="panel-placeholder">
      <h2>Diff</h2>
      <p>wkrótce — liniowy + semantyczny</p>
    </div>
  );
}
