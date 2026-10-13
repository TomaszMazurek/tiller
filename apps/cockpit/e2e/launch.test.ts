// @vitest-environment node
import { execFileSync } from 'node:child_process';
import { existsSync, rmSync } from 'node:fs';
import { createServer, type Server } from 'node:http';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, describe, expect, it } from 'vitest';
import { launchCockpit, makeFixtureRepo, pickFreeCdpPort, waitForCdp, type LaunchFailure } from './launch';

// Helper unit tests (plain node — no Playwright here): the launch
// harness must be verifiable without spawning the release exe.

const servers: Server[] = [];

/** Minimal fake of WebView2's CDP HTTP endpoint: any request answers
 *  `/json/version`-shaped JSON. */
async function startFakeCdp(): Promise<number> {
  const server = createServer((_req, res) => {
    res.writeHead(200, { 'content-type': 'application/json' });
    res.end(JSON.stringify({ Browser: 'fake', webSocketDebuggerUrl: 'ws://127.0.0.1/fake' }));
  });
  servers.push(server);
  await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));
  return (server.address() as { port: number }).port;
}

afterEach(async () => {
  await Promise.all(
    servers.splice(0).map((server) => new Promise<void>((resolve) => server.close(() => resolve()))),
  );
});

describe('waitForCdp', () => {
  it('resolves with the /json/version answer once the debugger responds', async () => {
    const port = await startFakeCdp();
    await expect(waitForCdp(port, 2000)).resolves.toMatchObject({
      webSocketDebuggerUrl: 'ws://127.0.0.1/fake',
    });
  });

  it('rejects with a timeout when nothing listens on the port', async () => {
    const port = 20000 + Math.floor(Math.random() * 20000);
    await expect(waitForCdp(port, 600)).rejects.toThrow(/not ready after 600 ms/);
  });
});

/** Definitely-free port right now: listen on 0, read the assigned
 *  port, release it. Used to build tiny ranges with a known free port. */
async function freePort(): Promise<number> {
  const probe = createServer();
  await new Promise<void>((resolve) => probe.listen(0, '127.0.0.1', resolve));
  const { port } = probe.address() as { port: number };
  await new Promise<void>((resolve) => probe.close(() => resolve()));
  return port;
}

describe('pickFreeCdpPort', () => {
  it('returns a bind-checkable (free) port from the range', async () => {
    const base = await freePort();
    const port = await pickFreeCdpPort(base, base + 9);
    expect(port).toBeGreaterThanOrEqual(base);
    expect(port).toBeLessThanOrEqual(base + 9);
  });

  it('skips an occupied port and picks the other one in the range', async () => {
    const base = await freePort();
    // base+0 is held by us, base+1 is the only free port left in range.
    const holder = createServer();
    servers.push(holder);
    await new Promise<void>((resolve) => holder.listen(base, '127.0.0.1', resolve));
    await expect(pickFreeCdpPort(base, base + 1)).resolves.toBe(base + 1);
  });

  it('rejects when the whole range is occupied', async () => {
    const base = await freePort();
    const holder = createServer();
    servers.push(holder);
    await new Promise<void>((resolve) => holder.listen(base, '127.0.0.1', resolve));
    await expect(pickFreeCdpPort(base, base)).rejects.toThrow(/no free CDP port/);
  });
});

describe('launchCockpit (failed start)', () => {
  it('rejects cleanly when the exe does not exist — no crash, no leftover WebView2 data dir', async () => {
    const exe = join(tmpdir(), `tiller-e2e-missing-${process.pid}.exe`);
    rmSync(exe, { force: true });
    const failure = await launchCockpit(39_899, 5_000, exe).then(
      () => undefined,
      (error: LaunchFailure) => error,
    );
    // Reaching this line at all proves the async spawn 'error' event did
    // not escape as an unhandled exception (the old crash mode).
    expect(failure).toBeInstanceOf(Error);
    expect(failure!.message).toContain('not found');
    // The isolated user-data folder was created, then removed before the
    // error propagated — a failed launch leaves nothing behind.
    expect(failure!.userDataDir).toBeDefined();
    expect(existsSync(failure!.userDataDir!)).toBe(false);
  });
});

describe('makeFixtureRepo', () => {
  it('creates a main-branch repo with one commit, and cleanup removes it', () => {
    const repo = makeFixtureRepo();
    try {
      const branch = execFileSync('git', ['rev-parse', '--abbrev-ref', 'HEAD'], {
        cwd: repo.path,
        encoding: 'utf8',
      }).trim();
      expect(branch).toBe('main');
      const log = execFileSync('git', ['log', '--oneline', '-1'], {
        cwd: repo.path,
        encoding: 'utf8',
      });
      expect(log).toContain('e2e fixture: initial commit');
    } finally {
      repo.cleanup();
    }
    expect(existsSync(repo.path)).toBe(false);
  });
});
