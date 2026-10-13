import { existsSync } from 'node:fs';
import { join } from 'node:path';
import { chromium, expect, test, type Browser, type Page } from '@playwright/test';
import { launchCockpit, makeFixtureRepo, pickFreeCdpPort, type CockpitProcess, type FixtureRepo } from './launch';

/**
 * P8b E2E smoke: the full stack in the *release* window — WebView2,
 * Tauri bridge, rpc → core, git worktrees, and the P8a task↔terminal
 * handoff. Runs only locally against a built release exe (see
 * e2e/README.md); CI wiring is a deliberate later slice.
 */

const TASK_ID = 'smoke-1';

let repo: FixtureRepo | null = null;
let cockpit: CockpitProcess | null = null;
let browser: Browser | null = null;
let page: Page | null = null;

test.beforeAll(async () => {
  repo = makeFixtureRepo();
  // Operator range for cockpit smoke (9300-9399). The port is bind-checked
  // before spawn: a purely random pick can land on an occupied port, and
  // waitForCdp would then answer a foreign debugger — driving the wrong
  // window. (The residual TOCTOU window is closed by the identity assert
  // right after connectOverCDP.)
  const cdpPort = await pickFreeCdpPort(9300, 9399);
  cockpit = await launchCockpit(cdpPort, 30_000);
  browser = await chromium.connectOverCDP(cockpit.wsEndpoint);
  const context = browser.contexts()[0];
  page = context.pages()[0] ?? (await context.waitForEvent('page', { timeout: 15_000 }));
  await page.waitForLoadState('domcontentloaded');
  // IDENTITY, before any UI interaction: if WebView2 ended up on a port
  // someone else grabbed after the pre-check, we would be driving that
  // foreign window. Fail here, loudly and early.
  await expect(page).toHaveTitle(/Tiller Cockpit/, { timeout: 10_000 });
});

test.afterAll(async () => {
  // Best-effort UI discard (the real backend path), then — ALWAYS —
  // disconnect, kill the exact PID tree, remove the fixture. A smoke
  // failure must never leave a window or a temp repo behind.
  try {
    if (page !== null) {
      page.on('dialog', (dialog) => void dialog.accept());
      const kill = page.getByTestId('terminal-kill');
      // release the worktree (a running pi holds its cwd on Windows)
      if (await kill.isEnabled({ timeout: 2000 }).catch(() => false)) {
        await kill.click();
      }
      const task = page.getByTestId('review-task').filter({ hasText: TASK_ID });
      if (await task.isVisible({ timeout: 2000 }).catch(() => false)) {
        await task.click();
        await page.getByTestId('review-discard').click();
        await expect(page.getByTestId('review-task')).toHaveCount(0, { timeout: 10_000 });
      }
    }
  } catch (error) {
    console.warn('smoke: UI discard failed (fixture removal still runs):', error);
  } finally {
    await browser?.close().catch(() => undefined);
    cockpit?.kill();
    repo?.cleanup();
  }
});

test('window is alive: the Terminal dock tab is visible', async () => {
  await expect(page!.getByText('Terminal', { exact: true }).first()).toBeVisible({ timeout: 15_000 });
});

test('status bar shows the core version over rpc', async () => {
  await expect(page!.getByTestId('status-version')).toContainText('core v', { timeout: 15_000 });
});

test('create task: queue entry, worktree on disk, pi session auto-start', async () => {
  // The render wait alone budgets 20 s (plus load/create/worktree waits)
  // — the config's 60 s would cut the hard assert short.
  test.setTimeout(120_000);
  const p = page!;

  await p.getByTestId('review-repo').fill(repo!.path);
  await p.getByTestId('review-load').click();
  // Load landed once Refresh unlocks (it stays disabled without a loaded repo).
  await expect(p.getByTestId('review-refresh')).toBeEnabled({ timeout: 15_000 });

  await p.getByTestId('review-new-task').fill(TASK_ID);
  await p.getByTestId('review-create').click();

  // (a) the queue lists the task
  await expect(p.getByTestId('review-task').filter({ hasText: TASK_ID })).toBeVisible({
    timeout: 15_000,
  });

  // (b) the worktree exists on disk — real git, not just UI state
  const worktree = join(repo!.path, '.tiller', 'worktrees', TASK_ID);
  const worktreeDeadline = Date.now() + 15_000;
  while (!existsSync(worktree) && Date.now() < worktreeDeadline) {
    await p.waitForTimeout(250);
  }
  expect(existsSync(worktree), `${worktree} should exist`).toBe(true);

  // (c) the P8a handoff auto-started pi in the worktree
  //     (boot costs no API calls — session file appears on first message)
  await expect(p.getByTestId('terminal-status')).toHaveText('running', { timeout: 15_000 });
  await expect(p.getByTestId('terminal-kill')).toBeEnabled();

  // (d) HARD: pi's bytes actually reached xterm. `.xterm-rows` existing
  //     proves nothing — the DOM renderer creates empty rows the moment
  //     the panel opens — so we require a row with real (non-whitespace)
  //     text: pi boots with a content screen (news/TUI frame); its very
  //     first frame can be a lone space (positioning write), which is not
  //     proof of a live stream. Regression guard for a dead term:output
  //     stream (a `running` status alone passed while the terminal stayed
  //     blank).
  const scanRows = () =>
    p.evaluate(() => {
      const rows = Array.from(document.querySelectorAll<HTMLElement>('.xterm-rows > div'));
      return {
        count: rows.length,
        withText: rows.filter((row) => (row.textContent ?? '').trim().length > 0).length,
      };
    });

  const startedAt = Date.now();
  const renderDeadline = startedAt + 20_000; // pi boot costs ~0.3-3 s; 20 s covers cold starts
  let backoff = 250;
  let nudged = false;
  let rowsSeen = false;
  let scan = await scanRows();
  while (scan.withText === 0 && Date.now() < renderDeadline) {
    if (!rowsSeen && scan.count > 0) {
      rowsSeen = true; // early info only — NOT a pass condition
      test.info().annotations.push({
        type: 'note',
        description: `xterm rows present after ${Date.now() - startedAt} ms (content still pending)`,
      });
    }
    if (!nudged && Date.now() - startedAt >= 8_000) {
      // Blank first screen (pi TUI variant): poke it — the TUI renders on
      // input. Click first, so the keyboard lands in the focused xterm.
      nudged = true;
      await p.locator('.xterm').first().click();
      await p.keyboard.press('Enter');
    }
    await p.waitForTimeout(backoff);
    backoff = Math.min(backoff * 2, 1_000);
    scan = await scanRows();
  }
  test.info().annotations.push({
    type: scan.withText > 0 ? 'note' : 'warning',
    description: `terminal text rendered after ${Date.now() - startedAt} ms${nudged ? ' (after Enter nudge)' : ''}`,
  });
  expect(
    scan.withText,
    `pi output should render into .xterm-rows within 20 s (rows: ${scan.count}, text rows: ${scan.withText}, nudged: ${nudged})`,
  ).toBeGreaterThan(0);
});
