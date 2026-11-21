import { paragraph } from '#lib/server/markdown.ts';

export function load() {
	return {
		intro: paragraph('README.md', null),
		promise: paragraph('README.md', 'What it does and does not promise')
	};
}
