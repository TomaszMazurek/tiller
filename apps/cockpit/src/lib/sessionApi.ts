/**
 * Typed helpers for the `session.*` rpc surface and its notification.
 *
 * Mirrors `termApi.ts`: the generic call path is `rpcCall`, these helpers
 * add the wire types. Event payloads use the backend's snake_case field
 * names — they mirror the `serde_json::json!` objects built in
 * `session_tail.rs` verbatim.
 */

import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { invoke } from '@tauri-apps/api/core';
import { rpcCall, type InvokeFn } from './rpcClient';
import type { SessionEntryEvent, TimelineEntry } from './timeline';
import type { SessionId, TermListenFn } from './termApi';

export async function sessionObserve(
  sessionId: SessionId,
  invokeFn: InvokeFn = invoke,
): Promise<TimelineEntry[]> {
  const result = (await rpcCall(
    'session.observe',
    { session_id: sessionId },
    invokeFn,
  )) as { entries?: unknown } | null;
  if (result === null || typeof result !== 'object' || !Array.isArray(result.entries)) {
    throw new Error('session.observe: result is missing an "entries" array');
  }
  return result.entries as TimelineEntry[];
}

export function subscribeSessionEntry(
  handler: (event: SessionEntryEvent) => void,
  listenFn: TermListenFn = listen,
): Promise<UnlistenFn> {
  return listenFn<SessionEntryEvent>('session:entry', (event) => handler(event.payload));
}
