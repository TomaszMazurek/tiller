import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { createResizeScheduler } from './resizeScheduler';

describe('createResizeScheduler', () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it('fires once after the quiet window', () => {
    const onResize = vi.fn();
    const scheduler = createResizeScheduler(onResize, 100);
    scheduler.schedule(80, 24);
    vi.advanceTimersByTime(99);
    expect(onResize).not.toHaveBeenCalled();
    vi.advanceTimersByTime(1);
    expect(onResize).toHaveBeenCalledTimes(1);
    expect(onResize).toHaveBeenCalledWith(80, 24);
  });

  it('coalesces a burst into the latest size', () => {
    const onResize = vi.fn();
    const scheduler = createResizeScheduler(onResize, 100);
    scheduler.schedule(80, 24);
    vi.advanceTimersByTime(50);
    scheduler.schedule(100, 30);
    vi.advanceTimersByTime(50);
    scheduler.schedule(120, 40);
    vi.advanceTimersByTime(99);
    expect(onResize).not.toHaveBeenCalled();
    vi.advanceTimersByTime(1);
    expect(onResize).toHaveBeenCalledTimes(1);
    expect(onResize).toHaveBeenCalledWith(120, 40);
  });

  it('cancel drops the pending resize', () => {
    const onResize = vi.fn();
    const scheduler = createResizeScheduler(onResize, 100);
    scheduler.schedule(80, 24);
    scheduler.cancel();
    vi.advanceTimersByTime(500);
    expect(onResize).not.toHaveBeenCalled();
  });
});
