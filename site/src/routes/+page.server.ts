import { block } from '#lib/server/markdown.ts';

export function load() {
	const intro = block('README.md', null, 'paragraph', 0);
	return {
		// README's first sentence, for search results and link previews.
		description: intro.text.replace(/\s+/g, ' ').split(/(?<=\.)\s/)[0],
		intro: intro.html,
		status: block('README.md', null, 'paragraph', 1).html,
		modes: block('README.md', 'Modes', 'list').html,
		promise: block('README.md', 'What it does and does not promise', 'paragraph').html
	};
}
