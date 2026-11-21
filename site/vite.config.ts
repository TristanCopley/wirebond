import adapter from '@sveltejs/adapter-static';
import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';

export default defineConfig({
	plugins: [
		sveltekit({
			adapter: adapter({ strict: true }),
			// Prerendered pages carry this policy as a <meta> tag: nothing loads from elsewhere,
			// and the only request that leaves the site is the download page's release refresh.
			csp: {
				mode: 'hash',
				directives: {
					'default-src': ['self'],
					'script-src': ['self'],
					'style-src': ['self'],
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
