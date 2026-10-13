import react from '@vitejs/plugin-react';
import { configDefaults, defineConfig } from 'vitest/config';

export default defineConfig({
  plugins: [react()],
  test: {
    environment: 'jsdom',
    setupFiles: ['src/test/setup.ts'],
    // e2e/smoke.spec.ts belongs to Playwright (CDP + release exe), not
    // vitest; e2e/launch.test.ts (plain node) stays in this run.
    exclude: [...configDefaults.exclude, 'e2e/smoke.spec.ts'],
  },
});
