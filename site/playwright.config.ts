import { defineConfig, devices } from '@playwright/test';

// The site has no Node code of its own, so it carries no Node types; this is all it reads.
declare const process: { env: Record<string, string | undefined> };

// Where Playwright's own Chromium is not installed, PW_CHROMIUM names one to use instead (in the
// sandboxes this site was built in: PW_CHROMIUM=/opt/pw-browsers/chromium).
const executablePath = process.env.PW_CHROMIUM;
// A build under BASE_PATH (GitHub Pages' /wirebond) is served and tested there.
const base = process.env.BASE_PATH ?? '';

export default defineConfig({
	testDir: 'tests',
	// The tests share only the preview server; each has its own page and its own mocked API.
	fullyParallel: true,
	forbidOnly: !!process.env.CI,
	reporter: 'list',
	use: {
		baseURL: `http://127.0.0.1:4173${base}/`,
		...devices['Desktop Chrome'],
		launchOptions: executablePath ? { executablePath } : {}
	},
	// Serves the production build exactly as prerendered: `npm test` tests whatever is in build/,
	// so run `npm run build` first.
	webServer: {
		command: 'npx vite preview --host 127.0.0.1 --port 4173 --strictPort',
		url: `http://127.0.0.1:4173${base}/`,
		reuseExistingServer: false
	}
});
