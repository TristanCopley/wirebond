import { doc } from '#lib/server/markdown.ts';

export function load() {
	return { doc: doc('README.md') };
}
