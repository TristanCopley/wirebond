// The latest GitHub release, read once at build time and, in the browser, once more when the
// download page opens. The same code runs in both places.

export const REPO = 'TristanCopley/wirebond';
export const LATEST_API = `https://api.github.com/repos/${REPO}/releases/latest`;
export const RELEASES = `https://github.com/${REPO}/releases`;

/** The daemon's assets (SPEC §22.1), named as `uname -sm` prints the machine. */
export const PLATFORMS = [
	{ os: 'Linux', machine: 'x86_64', asset: 'wirebond-Linux-x86_64' },
	{ os: 'Linux', machine: 'aarch64', asset: 'wirebond-Linux-aarch64' },
	{ os: 'Linux', machine: 'armv7l', asset: 'wirebond-Linux-armv7l' },
	{ os: 'Linux', machine: 'armv6l', asset: 'wirebond-Linux-armv6l' },
	{ os: 'macOS', machine: 'arm64', asset: 'wirebond-Darwin-arm64' },
	{ os: 'macOS', machine: 'x86_64', asset: 'wirebond-Darwin-x86_64' },
	{ os: 'Windows', machine: 'AMD64', asset: 'wirebond-Windows-AMD64.exe' },
	{ os: 'Windows', machine: 'ARM64', asset: 'wirebond-Windows-ARM64.exe' }
] as const;

export const SUMS = 'SHA256SUMS';

export type Asset = { name: string; url: string; size: number };
export type Release = {
	tag: string;
	url: string;
	published: string | null;
	assets: Record<string, Asset>;
};

/**
 * `found`: a release. `none`: GitHub answered that there is no release. `unreachable`: no answer
 * worth trusting (no network, a timeout, a rate limit); the page then says what it says for `none`.
 */
export type Latest =
	| { state: 'found'; release: Release }
	| { state: 'none' }
	| { state: 'unreachable'; reason: string };

function parse(json: unknown): Release | null {
	if (typeof json !== 'object' || json === null) return null;
	const r = json as Record<string, unknown>;
	if (typeof r.tag_name !== 'string' || typeof r.html_url !== 'string') return null;
	const assets: Record<string, Asset> = {};
	for (const a of Array.isArray(r.assets) ? r.assets : []) {
		if (typeof a?.name === 'string' && typeof a?.browser_download_url === 'string') {
			assets[a.name] = { name: a.name, url: a.browser_download_url, size: Number(a.size) || 0 };
		}
	}
	return {
		tag: r.tag_name,
		url: r.html_url,
		published: typeof r.published_at === 'string' ? r.published_at : null,
		assets
	};
}

/** `headers` adds to the request's own; the build passes its token there, never the browser. */
export async function latest(headers: Record<string, string> = {}, timeoutMs = 8000): Promise<Latest> {
	let res: Response;
	try {
		res = await fetch(LATEST_API, {
			headers: { accept: 'application/vnd.github+json', ...headers },
			signal: AbortSignal.timeout(timeoutMs)
		});
	} catch (err) {
		// Node's fetch says only "fetch failed"; the cause names it (ENOTFOUND, ECONNREFUSED...).
		const cause = err instanceof Error && err.cause instanceof Error ? `: ${err.cause.message}` : '';
		return { state: 'unreachable', reason: `${err instanceof Error ? err.message : String(err)}${cause}` };
	}
	if (res.status === 404) return { state: 'none' };
	const remaining = res.headers.get('x-ratelimit-remaining');
	if (res.status === 429 || (res.status === 403 && remaining === '0')) {
		return { state: 'unreachable', reason: `rate limited: HTTP ${res.status}, x-ratelimit-remaining ${remaining}` };
	}
	if (!res.ok) return { state: 'unreachable', reason: `HTTP ${res.status}` };
	try {
		const release = parse(await res.json());
		return release ? { state: 'found', release } : { state: 'unreachable', reason: 'unexpected answer' };
	} catch {
		return { state: 'unreachable', reason: 'unreadable answer' };
	}
}
