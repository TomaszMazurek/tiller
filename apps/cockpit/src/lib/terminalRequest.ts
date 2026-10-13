import { createContext, useContext } from 'react';

/**
 * The task↔terminal handoff channel (P8a).
 *
 * Why a context and not panel props: dockview captures the component
 * function once, when a panel is created (`ReactPanelContentPart`), and
 * the components map App passes at mount is frozen in the dockview
 * instance — so prop changes on App's factories never reach a panel that
 * is already mounted (the terminal mounted at startup kept a stale
 * `startRequest=null` forever; P8a review blocker). Panels render through
 * React portals inside `<DockviewReact>`'s subtree, so a provider above
 * it reaches every mounted panel regardless of the frozen factory.
 *
 * The request travels as data, never a direct panel-to-panel call, and is
 * cleared the moment the terminal handled it (`consume`) — it cannot
 * replay on later renders.
 */

/** A task↔terminal handoff request: what to run, where. */
export interface TerminalStartRequest {
  program: string;
  cwd: string;
}

/** The channel a provider (App) publishes and the terminal consumes. */
export interface TerminalRequestChannel {
  /** Pending request; `null` ⇔ nothing to start. */
  request: TerminalStartRequest | null;
  /** Acknowledge handling: the provider clears the request. */
  consume(): void;
}

export const TerminalRequestContext = createContext<TerminalRequestChannel | null>(null);

/** Read the handoff channel; throws outside a provider — a missing
 *  provider means Review clicks would silently do nothing (exactly the
 *  bug class this channel exists to prevent), so it fails loudly. */
export function useTerminalRequest(): TerminalRequestChannel {
  const channel = useContext(TerminalRequestContext);
  if (channel === null) {
    throw new Error('useTerminalRequest: <TerminalRequestContext.Provider> is missing');
  }
  return channel;
}
