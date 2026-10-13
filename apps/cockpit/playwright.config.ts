import { defineConfig } from '@playwright/test';

// CDP smoke against the *release* exe: there are no managed-browser
// projects here — e2e/launch.ts spawns the app and the spec attaches
// with `chromium.connectOverCDP` (a real WebView2, not a bundled
// Chromium). One worker, no retries: a smoke must be honest.
export default defineConfig({
  testDir: 'e2e',
  // only *.spec.ts — the vitest unit tests (launch.test.ts) live here too
  testMatch: '**/*.spec.ts',
  fullyParallel: false,
  workers: 1,
  timeout: 60_000,
  retries: 0,
  reporter: [['list']],
});
