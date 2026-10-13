/**
 * Session event gate — the fix for the listen-before-spawn race.
 *
 * `term.spawn` is an async rpc: the backend can emit the session's whole
 * `term:output` and even `term:exit` before the spawn call resolves with
 * the session id, so the UI must subscribe *before* spawning and then
 * match events against an id it does not know yet. The gate is that
 * matcher as pure, DOM-free logic:
 *
 * - `arm()` marks a spawn in flight: events are buffered (id unknown).
 * - `adopt(id)` resolves the id: buffered events replay in arrival
 *   order, but only those matching `id` — anything else in the buffer
 *   was another session's noise and is dropped.
 * - After adoption, matching events apply live, foreign ids are dropped.
 * - Unarmed and unadopted (idle panel), every event is dropped.
 */

import type { SessionId } from './termApi';

/** Anything a `term.*` notification carries on the wire. */
export interface SessionEvent {
  session_id: SessionId;
}

export class SessionEventGate<E extends SessionEvent> {
  private adoptedId: SessionId | null = null;
  private armed = false;
  private buffered: E[] = [];

  constructor(private readonly apply: (event: E) => void) {}

  /** A spawn for this gate is in flight: start buffering. */
  arm(): void {
    this.armed = true;
  }

  /** The spawn attempt is over without an id (failure): drop the buffer. */
  disarm(): void {
    this.armed = false;
    this.buffered = [];
  }

  /** The spawn resolved: adopt its id and replay the matching buffer. */
  adopt(id: SessionId): void {
    this.adoptedId = id;
    this.armed = false;
    const pending = this.buffered;
    this.buffered = [];
    for (const event of pending) {
      if (event.session_id === id) {
        this.apply(event);
      }
    }
  }

  /** Stop applying events (session over / teardown). */
  release(): void {
    this.adoptedId = null;
    this.armed = false;
    this.buffered = [];
  }

  /** Route one notification: apply, buffer, or drop (see module docs). */
  onEvent(event: E): void {
    if (this.adoptedId !== null) {
      if (event.session_id === this.adoptedId) {
        this.apply(event);
      }
      return;
    }
    if (this.armed) {
      this.buffered.push(event);
    }
  }
}
