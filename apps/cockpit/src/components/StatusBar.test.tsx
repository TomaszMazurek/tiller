import { render, screen } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';

type InvokeMock = (command: string, args?: Record<string, unknown>) => Promise<unknown>;

const invoke = vi.fn<InvokeMock>();

vi.mock('@tauri-apps/api/core', () => ({
  invoke: (...args: Parameters<typeof invoke>) => invoke(...args),
}));

import { StatusBar } from './StatusBar';

function respondWith(payload: unknown) {
  invoke.mockResolvedValue(JSON.stringify(payload));
}

describe('StatusBar', () => {
  beforeEach(() => {
    invoke.mockReset();
  });

  it('shows the core version fetched over rpc', async () => {
    respondWith({ id: 1, result: { version: '0.1.0' } });
    render(<StatusBar />);
    expect(await screen.findByText('core v0.1.0')).toBeDefined();
    expect(invoke).toHaveBeenCalledWith(
      'rpc',
      expect.objectContaining({ json: expect.stringContaining('"core.version"') }),
    );
  });

  it('shows an rpc error instead of crashing', async () => {
    respondWith({ id: 1, error: { code: -32601, message: 'unknown method: nope' } });
    render(<StatusBar />);
    expect(await screen.findByText(/rpc error: unknown method: nope/i)).toBeDefined();
  });
});
