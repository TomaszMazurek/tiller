/**
 * Trailing-edge debounce for terminal resize: xterm `fit()` plus a
 * `term.resize` rpc per layout change would storm the backend while a
 * dockview panel is being dragged — one call per quiet window is enough.
 */

export interface ResizeScheduler {
  /** Schedule a resize; only the latest size fires, after the quiet window. */
  schedule(cols: number, rows: number): void;
  /** Drop any pending resize (unmount, session gone). */
  cancel(): void;
}

export function createResizeScheduler(
  onResize: (cols: number, rows: number) => void,
  delayMs = 100,
): ResizeScheduler {
  let timer: ReturnType<typeof setTimeout> | null = null;
  let pending: { cols: number; rows: number } | null = null;
  return {
    schedule(cols, rows) {
      pending = { cols, rows };
      if (timer !== null) {
        clearTimeout(timer);
      }
      timer = setTimeout(() => {
        timer = null;
        const next = pending;
        pending = null;
        if (next) {
          onResize(next.cols, next.rows);
        }
      }, delayMs);
    },
    cancel() {
      if (timer !== null) {
        clearTimeout(timer);
        timer = null;
      }
      pending = null;
    },
  };
}
