import { defineConfig, devices } from '@playwright/test';

// scripts/console-e2e.sh starts the real collector (serving the built console), the
// reference host and the data; this config only points the browser at them.
export default defineConfig({
	testDir: 'tests',
	workers: 1,
	fullyParallel: false,
	retries: 0,
	reporter: [['list']],
	timeout: 30_000,
	use: {
		baseURL: process.env['AIOPS_E2E_BASE'] ?? 'http://127.0.0.1:57420',
		trace: 'retain-on-failure'
	},
	projects: [{ name: 'chromium', use: { ...devices['Desktop Chrome'] } }]
});
