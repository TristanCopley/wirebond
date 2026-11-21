import { defineConfig, devices } from '@playwright/test';

// The site has no Node code of its own, so it carries no Node types; this is all it reads.
declare const process: { env: Record<string, string | undefined> };

// Where Playwright's own Chromium is not installed, PW_CHROMIUM names one to use instead.
const executablePath = process.env.PW_CHROMIUM;

export default defineConfig({
	testDir: 'tests',
	forbidOnly: !!process.env.CI,
	reporter: 'list',
	use: {
		baseURL: 'http://127.0.0.1:4173',
		...devices['Desktop Chrome'],
		launchOptions: executablePath ? { executablePath } : {}
	},
	// Serves the production build (run `npm run build` first) exactly as prerendered.
	webServer: {
		command: 'npx vite preview --host 127.0.0.1 --port 4173 --strictPort',
		url: 'http://127.0.0.1:4173',
		reuseExistingServer: false
	}
});
