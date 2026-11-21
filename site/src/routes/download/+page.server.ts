import { code, section } from '#lib/server/markdown.ts';
import { latest, LATEST_API, type Latest } from '#lib/release.ts';

let checked: Promise<Latest> | undefined;

// Asked once per build, and said in the build log, since the page reads the same either way
// when there is no release to offer.
function check(): Promise<Latest> {
	checked ??= latest().then((result) => {
		if (result.state === 'found') console.log(`[release] ${LATEST_API}: ${result.release.tag}`);
		else if (result.state === 'none') console.log(`[release] ${LATEST_API}: none published (404)`);
		else console.log(`[release] ${LATEST_API}: unreachable (${result.reason}); built as if none`);
		return result;
	});
	return checked;
}

export async function load() {
	const result = await check();
	const installers = code('README.md', 'Install');
	return {
		release: result.state === 'found' ? result.release : null,
		sh: installers.sh,
		powershell: installers.powershell,
		build: section('README.md', 'Build and test')
	};
}
