import { doc } from '#lib/server/markdown.ts';

export function load() {
	return { doc: doc('SPEC.md') };
}
