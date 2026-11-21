import { section } from '#lib/server/markdown.ts';
import { latest, LATEST_API, type Latest } from '#lib/release.ts';

// The build runs in Node; the site carries no Node types, so this is all it reads.
declare const process: { env: Record<string, string | undefined> };

let checked: Promise<Latest> | undefined;

// Asked once per build, and said in the build log, since the page reads the same either way
// when there is no release to offer. On CI, GITHUB_TOKEN lifts GitHub's limit for anonymous
// callers, which a shared build machine has often used up; a token GitHub refuses (401) is dropped.
async function ask(): Promise<Latest> {
	const token = process.env.GITHUB_TOKEN;
	const result = token ? await latest({ authorization: `Bearer ${token}` }) : undefined;
	if (result && !(result.state === 'unreachable' && result.reason === 'HTTP 401')) return result;
	if (result) console.log('[release] GitHub refused GITHUB_TOKEN (401); asking without it');
	return latest();
}

function check(): Promise<Latest> {
	checked ??= ask().then((result) => {
		if (result.state === 'found') console.log(`[release] ${LATEST_API}: ${result.release.tag}`);
		else if (result.state === 'none') console.log(`[release] ${LATEST_API}: none published (404)`);
		else console.log(`[release] ${LATEST_API}: unreachable (${result.reason}); built as if none`);
		return result;
	});
	return checked;
}

export async function load() {
	const result = await check();
	return {
		release: result.state === 'found' ? result.release : null,
		built: new Date().toISOString().slice(0, 10),
		install: section('README.md', 'Install'),
		build: section('README.md', 'Build and test')
	};
}
