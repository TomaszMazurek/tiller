import { createContext, useContext } from 'react';
import type { SessionId } from './termApi';

/**
 * The active terminal session channel (P8a-2).
 *
 * Why a context and not panel props: dockview captures the component
 * function once, when a panel is created (`ReactPanelContentPart`), and
 * the components map App passes at mount is frozen in the dockview
 * instance — so prop changes on App's factories never reach a panel that
 * is already mounted (the timeline mounted at startup kept a stale
 * `activeSessionId=null` forever; the timeline was dead while a live
 * session ran). Panels render through React portals inside
 * `<DockviewReact>`'s subtree, so a provider above it reaches every
 * mounted panel regardless of the frozen factory — see
 * `lib/terminalRequest.ts` for the sibling channel (P8a).
 *
 * Only the id travels here — no setter: the source of truth stays in
 * App, where TerminalPanel reports the id upward over a stable callback
 * prop (identity never changes, which the frozen factory *does*
 * deliver).
 */

/** The session a provider (App) publishes and the timeline consumes. */
export interface ActiveSession {
  /** The terminal session whose session file is observed; `null` ⇔ none. */
  sessionId: SessionId | null;
}

export const ActiveSessionContext = createContext<ActiveSession | null>(null);

/** Read the active session; throws outside a provider — a missing
 *  provider would silently pin the timeline at "no active session"
 *  while a live session runs (exactly the bug class this channel
 *  exists to prevent), so it fails loudly. */
export function useActiveSession(): ActiveSession {
  const session = useContext(ActiveSessionContext);
  if (session === null) {
    throw new Error('useActiveSession: <ActiveSessionContext.Provider> is missing');
  }
  return session;
}
