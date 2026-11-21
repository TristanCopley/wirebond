<script lang="ts">
	import '../app.css';
	import type { Snippet } from 'svelte';
	import { page } from '$app/state';
	import { resolve } from '$app/paths';

	let { children }: { children: Snippet } = $props();

	const nav = [
		{ route: '/download', label: 'Download' },
		{ route: '/docs', label: 'Docs' },
		{ route: '/docs/spec', label: 'Spec' }
	] as const;
</script>

<a class="skip" href="#main">Skip to content</a>

<header class="site">
	<nav aria-label="Site">
		<a class="brand" href={resolve('/')} aria-current={page.route.id === '/' ? 'page' : undefined}
			>wirebond</a
		>
		<ul>
			{#each nav as item (item.route)}
				<li>
					<a href={resolve(item.route)} aria-current={page.route.id === item.route ? 'page' : undefined}
						>{item.label}</a
					>
				</li>
			{/each}
			<li><a href="https://github.com/TristanCopley/wirebond">GitHub</a></li>
		</ul>
	</nav>
</header>

<main id="main" tabindex="-1">
	{@render children()}
</main>

<footer class="site">
	<p>
		wirebond is developed in the open on
		<a href="https://github.com/TristanCopley/wirebond">GitHub</a>. This site sets no cookies and
		runs no analytics.
	</p>
</footer>
