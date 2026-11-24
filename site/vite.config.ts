import adapter from '@sveltejs/adapter-static';
import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';

// The site's path on its host: empty at a domain's root (and for npm run dev), "/wirebond" for
// GitHub Pages' project site at tristancopley.github.io/wirebond/. site.yml sets it.
declare const process: { env: Record<string, string | undefined> };
const base = process.env.BASE_PATH ?? '';
if (base !== '' && !/^\/[^/].*[^/]$|^\/[^/]$/.test(base)) {
	throw new Error(`BASE_PATH must be empty or start with / and not end with one, not ${JSON.stringify(base)}`);
}

export default defineConfig({
	plugins: [
		sveltekit({
			adapter: adapter({ strict: true }),
			paths: { base: base as '' | `/${string}` },
			// The one stylesheet, under 2 KB compressed, goes into each page, so that nothing
			// holds up the first paint; the policy below allows it by its hash.
			inlineStyleThreshold: Infinity,
			// Prerendered pages carry this policy as a <meta> tag: nothing loads from elsewhere,
			// and the only request that leaves the site is the download page's release refresh.
			csp: {
				mode: 'hash',
				directives: {
					'default-src': ['self'],
					'script-src': ['self'],
					'style-src': ['self'],
					// SvelteKit's route announcer, mounted on the download page, is hidden with a style
					// attribute. Attributes only: style elements and scripts stay locked.
					'style-src-attr': ['unsafe-inline'],
					'img-src': ['self', 'data:'],
					'connect-src': ['self', 'https://api.github.com'],
					'base-uri': ['self'],
					'form-action': ['none'],
					'object-src': ['none']
				}
			},
			prerender: {
				// Every in-page link, the docs' tables of contents and § references included,
				// must land on a heading that exists.
				handleMissingId: 'fail'
			}
		})
	],
	// The docs are read from the repository root, one level up.
	server: { fs: { allow: ['..'] } }
});
