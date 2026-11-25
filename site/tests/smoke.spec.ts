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

// The tests run against a build under BASE_PATH when it is set, as GitHub Pages serves it.
declare const process: { env: Record<string, string | undefined> };
const BASE = process.env.BASE_PATH ?? '';
const at = (path: string) => `${BASE}${path}`;

// A published release, as GitHub answers since v2.0.0-rc.1. Tests never reach the real API:
// each one that needs another answer replaces this one.
function release(tag: string) {
	return {
		tag_name: tag,
		html_url: `https://github.com/TristanCopley/wirebond/releases/tag/${tag}`,
		published_at: '2030-01-02T03:04:05Z',
		assets: [...ASSETS, 'SHA256SUMS'].map((name) => ({
			name,
			size: 2_000_000,
			browser_download_url: `https://github.com/TristanCopley/wirebond/releases/download/${tag}/${name}`
		}))
	};
}

test.beforeEach(async ({ page }) => {
	await page.route(API, (route) => route.fulfill({ json: release('v9.9.9') }));
});

for (const { path, heading } of ROUTES) {
	test(`${path} renders its main heading`, async ({ page }) => {
		// The stylesheet is inline and allowed by its hash in the page's policy: a hash that did
		// not match would leave the page unstyled, with this error on the console.
		const csp: string[] = [];
		page.on('console', (m) => {
			if (m.type() === 'error' && /Content Security Policy/i.test(m.text())) csp.push(m.text());
		});
		await page.goto(at(path));
		await expect(page.getByRole('heading', { level: 1 })).toHaveText(heading);
		await expect(page.getByRole('main')).toBeVisible();
		await expect(page.locator('html')).toHaveAttribute('lang', 'en');
		expect(csp).toEqual([]);
	});

	test(`${path} has no horizontal scroll at 320px`, async ({ page }) => {
		await page.setViewportSize({ width: 320, height: 640 });
		await page.goto(at(path));
		const overflow = await page.evaluate(
			() => document.documentElement.scrollWidth - document.documentElement.clientWidth
		);
		expect(overflow).toBe(0);
	});
}

const NONE = 'No release had been published when this page was built';

test('the download page says so when GitHub has no release', async ({ page }) => {
	await page.unroute(API);
	await page.route(API, (route) =>
		route.fulfill({ status: 404, contentType: 'application/json', body: '{"message":"Not Found"}' })
	);
	const csp: string[] = [];
	page.on('console', (m) => {
		if (m.type() === 'error' && /Content Security Policy/i.test(m.text())) csp.push(m.text());
	});
	await page.goto(at('/download'), { waitUntil: 'networkidle' });
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
	await page.goto(at('/download'));
	await expect(page.getByRole('link', { name: 'v9.9.9' })).toBeVisible();
	await expect(page.getByText(NONE)).toHaveCount(0);
	await expect(page.getByRole('link', { name: 'wirebond-Darwin-arm64' })).toHaveAttribute(
		'href',
		/download\/v9\.9\.9\/wirebond-Darwin-arm64$/
	);
});

test('the spec renders its sections, contents and § links', async ({ page }) => {
	await page.goto(at('/docs/spec'));
	for (const name of ['1. Goals, non-goals and the promise', '19. CLI', '22. Install, update and platforms']) {
		await expect(page.getByRole('heading', { level: 2, name, exact: true })).toBeVisible();
	}
	await expect(page.locator('h3[id="221-platforms-and-assets"]')).toHaveText('22.1 Platforms and assets');
	await expect(page.getByRole('navigation', { name: 'Contents' }).locator('a[href="#19-cli"]')).toHaveCount(1);
	await expect(page.locator('a[href$="/docs/spec#23-testing-and-acceptance"]', { hasText: '§23' }).first()).toBeVisible();
	await expect(page.locator('table').first()).toBeVisible();
});

test('links between README and SPEC stay on the site', async ({ page }) => {
	await page.goto(at('/docs'));
	await page.getByRole('main').getByRole('link', { name: 'SPEC.md', exact: true }).first().click();
	await expect(page).toHaveURL(new RegExp(`${BASE}/docs/spec$`));
	await page.goto(at('/docs'));
	await expect(page.getByRole('link', { name: 'NOTES.md', exact: true })).toHaveAttribute(
		'href',
		/^https:\/\/github\.com\/TristanCopley\/wirebond\/blob\/[^/]+\/NOTES\.md$/
	);
});

test('the first Tab reaches the skip link, and the text comes before the contents', async ({ page }) => {
	await page.goto(at('/docs/spec'));
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
	for (const path of ['/', '/docs', '/docs/spec']) await page.goto(at(path), { waitUntil: 'networkidle' });
	expect(foreign).toEqual([]);
});

test.describe('with JavaScript disabled', () => {
	test.use({ javaScriptEnabled: false });

	test('every page still reads', async ({ page }) => {
		for (const { path, heading } of ROUTES) {
			await page.goto(at(path));
			await expect(page.getByRole('heading', { level: 1 })).toHaveText(heading);
		}
		// What the page says about a release depends on GitHub's answer when it was built.
		await page.goto(at('/download'));
		await expect(page.locator('pre code.language-sh').first()).toContainText('install.sh | sh');
		await expect(page.locator('table.assets code', { hasText: /^wirebond-Linux-x86_64$/ })).toBeVisible();
		await page.goto(at('/docs/spec'));
		await expect(page.getByRole('heading', { level: 2, name: '19. CLI' })).toBeVisible();
	});
});

test('every link within the site leads to a page under the base path', async ({ page, baseURL }) => {
	const origin = new URL(baseURL!).origin;
	const seen = new Set<string>();
	for (const { path } of ROUTES) {
		await page.goto(at(path));
		const hrefs = await page.locator('a[href]').evaluateAll((as) => as.map((a) => (a as HTMLAnchorElement).href));
		for (const href of hrefs) {
			const url = new URL(href);
			if (url.origin !== origin) continue;
			url.hash = '';
			seen.add(url.href);
		}
	}
	expect(seen.size).toBeGreaterThanOrEqual(ROUTES.length);
	for (const href of seen) {
		expect(new URL(href).pathname.startsWith(`${BASE}/`), href).toBe(true);
		const response = await page.request.get(href);
		expect(response.status(), href).toBe(200);
	}
});
