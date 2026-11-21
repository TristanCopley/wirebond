import { expect, test, type Page } from '@playwright/test';

const ROUTES = [
	{ path: '/', heading: 'wirebond' },
	{ path: '/download', heading: 'Download' },
	{ path: '/docs', heading: 'wirebond' },
	{ path: '/docs/spec', heading: 'wirebond 2.0 specification' }
];

const ASSETS = [
	'wirebond-Linux-x86_64',
	'wirebond-Linux-aarch64',
	'wirebond-Linux-armv7l',
	'wirebond-Linux-armv6l',
	'wirebond-Darwin-arm64',
	'wirebond-Darwin-x86_64',
	'wirebond-Windows-AMD64.exe',
	'wirebond-Windows-ARM64.exe'
];

const API = 'https://api.github.com/repos/TristanCopley/wirebond/releases/latest';

// GitHub as it answers today: no release. Tests never reach the real API.
async function noRelease(page: Page) {
	await page.route(API, (route) =>
		route.fulfill({ status: 404, contentType: 'application/json', body: '{"message":"Not Found"}' })
	);
}

test.beforeEach(async ({ page }) => noRelease(page));

for (const { path, heading } of ROUTES) {
	test(`${path} renders its main heading`, async ({ page }) => {
		await page.goto(path);
		await expect(page.getByRole('heading', { level: 1 })).toHaveText(heading);
		await expect(page.getByRole('main')).toBeVisible();
		await expect(page.locator('html')).toHaveAttribute('lang', 'en');
	});

	test(`${path} has no horizontal scroll at 320px`, async ({ page }) => {
		await page.setViewportSize({ width: 320, height: 640 });
		await page.goto(path);
		const overflow = await page.evaluate(
			() => document.documentElement.scrollWidth - document.documentElement.clientWidth
		);
		expect(overflow).toBe(0);
	});
}

const NONE = 'No release had been published when this page was built';

test('the download page says no release has been published', async ({ page }) => {
	const csp: string[] = [];
	page.on('console', (m) => {
		if (m.type() === 'error' && /Content Security Policy/i.test(m.text())) csp.push(m.text());
	});
	await page.goto('/download', { waitUntil: 'networkidle' });
	await expect(page.getByText(NONE)).toBeVisible();
	await expect(page.locator('pre code.language-sh').first()).toContainText('install.sh | sh');
	await expect(page.locator('pre code.language-powershell')).toContainText('install.ps1 | iex');
	for (const asset of [...ASSETS, 'SHA256SUMS']) {
		await expect(page.getByRole('cell', { name: asset, exact: true })).toBeVisible();
	}
	await expect(page.getByRole('heading', { name: 'Build from source' })).toBeVisible();
	// Whatever README's build steps are, they are rendered here.
	await expect(page.locator('#build ~ pre code').first()).not.toBeEmpty();
	expect(csp).toEqual([]);
});

test('the download page links a release once GitHub has one', async ({ page }) => {
	await page.unroute(API);
	await page.route(API, (route) =>
		route.fulfill({
			json: {
				tag_name: 'v9.9.9',
				html_url: 'https://github.com/TristanCopley/wirebond/releases/tag/v9.9.9',
				published_at: '2030-01-02T03:04:05Z',
				assets: [...ASSETS, 'SHA256SUMS'].map((name) => ({
					name,
					size: 2_000_000,
					browser_download_url: `https://github.com/TristanCopley/wirebond/releases/download/v9.9.9/${name}`
				}))
			}
		})
	);
	await page.goto('/download');
	await expect(page.getByRole('link', { name: 'v9.9.9' })).toBeVisible();
	await expect(page.getByText(NONE)).toHaveCount(0);
	await expect(page.getByRole('link', { name: 'wirebond-Darwin-arm64' })).toHaveAttribute(
		'href',
		/download\/v9\.9\.9\/wirebond-Darwin-arm64$/
	);
});

test('the spec renders its sections, contents and § links', async ({ page }) => {
	await page.goto('/docs/spec');
	for (const name of ['1. Goals, non-goals and the promise', '19. CLI', '22. Install, update and platforms']) {
		await expect(page.getByRole('heading', { level: 2, name, exact: true })).toBeVisible();
	}
	await expect(page.locator('h3[id="221-platforms-and-assets"]')).toHaveText('22.1 Platforms and assets');
	await expect(page.getByRole('navigation', { name: 'Contents' }).locator('a[href="#19-cli"]')).toHaveCount(1);
	await expect(page.locator('a[href$="/docs/spec#23-testing-and-acceptance"]', { hasText: '§23' }).first()).toBeVisible();
	await expect(page.locator('table').first()).toBeVisible();
});

test('links between README and SPEC stay on the site', async ({ page }) => {
	await page.goto('/docs');
	await page.getByRole('main').getByRole('link', { name: 'SPEC.md', exact: true }).first().click();
	await expect(page).toHaveURL(/\/docs\/spec$/);
	await page.goto('/docs');
	await expect(page.getByRole('link', { name: 'NOTES.md', exact: true })).toHaveAttribute(
		'href',
		/^https:\/\/github\.com\/TristanCopley\/wirebond\/blob\/[^/]+\/NOTES\.md$/
	);
});

test('the first Tab reaches the skip link, and the text comes before the contents', async ({ page }) => {
	await page.goto('/docs/spec');
	await page.keyboard.press('Tab');
	await expect(page.getByRole('link', { name: 'Skip to content' })).toBeFocused();
	const order = await page.evaluate(() => {
		const article = document.querySelector('main article')!;
		const toc = document.querySelector('main nav.toc')!;
		return article.compareDocumentPosition(toc) & Node.DOCUMENT_POSITION_FOLLOWING;
	});
	expect(order).toBeTruthy();
});

test('pages other than download request nothing from elsewhere', async ({ page, baseURL }) => {
	const foreign: string[] = [];
	page.on('request', (r) => {
		if (!r.url().startsWith(baseURL!)) foreign.push(r.url());
	});
	for (const path of ['/', '/docs', '/docs/spec']) await page.goto(path, { waitUntil: 'networkidle' });
	expect(foreign).toEqual([]);
});

test.describe('with JavaScript disabled', () => {
	test.use({ javaScriptEnabled: false });

	test('every page still reads', async ({ page }) => {
		for (const { path, heading } of ROUTES) {
			await page.goto(path);
			await expect(page.getByRole('heading', { level: 1 })).toHaveText(heading);
		}
		// What the page says about a release depends on GitHub's answer when it was built.
		await page.goto('/download');
		await expect(page.locator('pre code.language-sh').first()).toContainText('install.sh | sh');
		await expect(page.locator('table.assets code', { hasText: /^wirebond-Linux-x86_64$/ })).toBeVisible();
		await page.goto('/docs/spec');
		await expect(page.getByRole('heading', { level: 2, name: '19. CLI' })).toBeVisible();
	});
});
