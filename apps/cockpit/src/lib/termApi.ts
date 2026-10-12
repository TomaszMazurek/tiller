/**
 * Typed helpers for the `term.*` rpc surface and its notifications.
 *
 * The generic call path is `rpcCall(method, params)` from `rpcClient.ts`;
 * these helpers add the term wire types and the base64 encoding of input
 * bytes. Event payloads use the backend's snake_case field names — they
 * mirror `serde_json::json!` objects built in `term_host.rs` verbatim.
 */

import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { invoke } from '@tauri-apps/api/core';
import { base64ToBytes, bytesToBase64 } from './base64';
import { rpcCall, type InvokeFn } from './rpcClient';

export type SessionId = number;

export interface TermSpawnParams {
  program: string;
  args?: string[];
  cwd: string;
  cols?: number;
  rows?: number;
  env?: Record<string, string>;
}

/** `term.output` notification payload: one base64-encoded output chunk. */
export interface TermOutputEvent {
  session_id: SessionId;
  data: string;
}

/** `term.exit` notification payload. */
export interface TermExitEvent {
  session_id: SessionId;
  exit_code: number;
  signal?: string | null;
  success: boolean;
  dsr_responses_sent: number;
}

export interface TermStats {
  dsr_responses_sent: number;
}

/** Something xterm-shaped that accepts bytes — a mock in tests. */
export interface ByteWriter {
  write(data: Uint8Array): void;
}

export async function termSpawn(
  params: TermSpawnParams,
  invokeFn: InvokeFn = invoke,
): Promise<SessionId> {
  const result = (await rpcCall('term.spawn', params, invokeFn)) as { session_id?: unknown } | null;
  if (result === null || typeof result !== 'object' || typeof result.session_id !== 'number') {
    throw new Error('term.spawn: result is missing a "session_id" number');
  }
  return result.session_id;
}

export async function termWrite(
  sessionId: SessionId,
  data: Uint8Array,
  invokeFn: InvokeFn = invoke,
): Promise<void> {
  await rpcCall(
    'term.write',
    { session_id: sessionId, data: bytesToBase64(data) },
    invokeFn,
  );
}

export async function termResize(
  sessionId: SessionId,
  cols: number,
  rows: number,
  invokeFn: InvokeFn = invoke,
): Promise<void> {
  await rpcCall('term.resize', { session_id: sessionId, cols, rows }, invokeFn);
}

export async function termKill(sessionId: SessionId, invokeFn: InvokeFn = invoke): Promise<void> {
  await rpcCall('term.kill', { session_id: sessionId }, invokeFn);
}

export async function termStats(
  sessionId: SessionId,
  invokeFn: InvokeFn = invoke,
): Promise<TermStats> {
  const result = (await rpcCall('term.stats', { session_id: sessionId }, invokeFn)) as {
    dsr_responses_sent?: unknown;
  } | null;
  if (result === null || typeof result !== 'object' || typeof result.dsr_responses_sent !== 'number') {
    throw new Error('term.stats: result is missing a "dsr_responses_sent" number');
  }
  return { dsr_responses_sent: result.dsr_responses_sent };
}

/** Decode one `term.output` payload into the terminal (pure mapping). */
export function applyTermOutput(target: ByteWriter, event: TermOutputEvent): void {
  target.write(base64ToBytes(event.data));
}

/** Injectable stand-in for `@tauri-apps/api/event.listen`. */
export type TermListenFn = <T>(
  event: string,
  handler: (event: { payload: T }) => void,
) => Promise<UnlistenFn>;

export function subscribeTermOutput(
  handler: (event: TermOutputEvent) => void,
  listenFn: TermListenFn = listen,
): Promise<UnlistenFn> {
  return listenFn<TermOutputEvent>('term.output', (event) => handler(event.payload));
}

export function subscribeTermExit(
  handler: (event: TermExitEvent) => void,
  listenFn: TermListenFn = listen,
): Promise<UnlistenFn> {
  return listenFn<TermExitEvent>('term.exit', (event) => handler(event.payload));
}
