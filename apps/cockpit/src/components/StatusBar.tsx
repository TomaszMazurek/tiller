import { useEffect, useState } from 'react';
import { coreVersion, RpcClientError } from '../lib/rpcClient';

interface StatusBarState {
  version: string | null;
  error: string | null;
}

/** Bottom bar of the cockpit; the P1 proof of the UI → rpc → core seam. */
export function StatusBar() {
  const [state, setState] = useState<StatusBarState>({ version: null, error: null });

  useEffect(() => {
    let cancelled = false;
    coreVersion()
      .then((version) => {
        if (!cancelled) setState({ version, error: null });
      })
      .catch((cause: unknown) => {
        if (cancelled) return;
        const message = cause instanceof RpcClientError || cause instanceof Error ? cause.message : String(cause);
        setState({ version: null, error: message });
      });
    return () => {
      cancelled = true;
    };
  }, []);

  return (
    <footer className="status-bar" data-testid="status-bar">
      <span className="status-bar__item status-bar__item--brand">Tiller</span>
      {state.version !== null && (
        <span className="status-bar__item" data-testid="status-version">
          core v{state.version}
        </span>
      )}
      {state.error !== null && <span className="status-bar__item status-bar__item--error">rpc error: {state.error}</span>}
    </footer>
  );
}
