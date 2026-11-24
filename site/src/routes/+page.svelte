<script lang="ts">
	import { resolve } from '$app/paths';
	import type { PageProps } from './$types';

	// Every sentence below the headings is README's own, rendered when the site is built.
	let { data }: PageProps = $props();
</script>

<svelte:head>
	<title>wirebond</title>
	<meta name="description" content={data.description} />
</svelte:head>

<div class="prose">
	<h1>wirebond</h1>
	{@html data.intro}

	<p class="actions">
		<a class="button" href={resolve('/download')}>Download</a>
		<a class="button quiet" href={resolve('/docs')}>Read the docs</a>
	</p>

	<section class="note" aria-labelledby="status">
		<h2 id="status">Status</h2>
		{@html data.status}
	</section>

	<h2>Two modes</h2>
	{@html data.modes}

	<h2>What it does and does not promise</h2>
	{@html data.promise}
	<p>
		The rest of what it does not aim for is in the specification's
		<a href="{resolve('/docs/spec')}#1-goals-non-goals-and-the-promise">non-goals</a>.
	</p>
</div>
