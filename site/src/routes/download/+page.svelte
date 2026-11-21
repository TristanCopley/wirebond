<script lang="ts">
	import { onMount } from 'svelte';
	import { resolve } from '$app/paths';
	import { latest, PLATFORMS, RELEASES, SUMS, type Release } from '#lib/release.ts';
	import type { PageProps } from './$types';

	// The install text and the build steps are README's, rendered when the site is built; the
	// asset table is SPEC §22.1's, linked to the latest release when there is one.
	let { data }: PageProps = $props();

	// undefined until the browser has asked GitHub itself; then its answer replaces the build's,
	// unless GitHub could not be reached.
	let live = $state<Release | null | undefined>(undefined);
	const release = $derived(live === undefined ? data.release : live);

	onMount(() => {
		latest().then((result) => {
			if (result.state === 'found') live = result.release;
			else if (result.state === 'none') live = null;
		});
	});

	const size = (bytes: number) => (bytes ? `${(bytes / 1e6).toFixed(1)} MB` : '');
	const day = (iso: string | null) => (iso ? iso.slice(0, 10) : '');
	const assets = `${resolve('/docs/spec')}#221-platforms-and-assets`;
</script>

<svelte:head>
	<title>Download · wirebond</title>
	<meta name="description" content="Install wirebond with one line, or download an executable." />
</svelte:head>

<div class="prose">
	<h1>Download</h1>

	<div aria-live="polite">
		{#if release}
			<p class="release">
				Latest release: <a href={release.url}><strong>{release.tag}</strong></a>{#if release.published},
					published <time datetime={release.published}>{day(release.published)}</time>{/if}.
			</p>
		{:else}
			<div class="note" id="no-release">
				<p>
					<strong>No release had been published when this page was built
						(<time datetime={data.built}>{data.built}</time>).</strong>
					<a href={RELEASES}>GitHub's releases</a> say whether that has changed.
				</p>
				<p>
					The installers download from the latest release, and the executables below are its
					assets, so both need a release to exist. Until then, <a href="#build">build from source</a>.
				</p>
			</div>
		{/if}
	</div>

	<h2>Install</h2>
	{@html data.install}
	<p>Then <a href="{resolve('/docs')}#set-up">set it up</a>.</p>

	<h2>Executables</h2>
	<p>The daemon's assets (<a href={assets}>SPEC §22.1</a>):</p>
	<div class="scroll">
		<table class="assets">
			<thead>
				<tr><th scope="col">Machine</th><th scope="col">Asset</th></tr>
			</thead>
			<tbody>
				{#each PLATFORMS as p (p.asset)}
					{@const a = release?.assets[p.asset]}
					<tr>
						<td>{p.os} <code>{p.machine}</code></td>
						<td>
							{#if a}
								<a href={a.url}><code>{p.asset}</code></a> <span class="muted">{size(a.size)}</span>
							{:else}
								<code>{p.asset}</code>
							{/if}
						</td>
					</tr>
				{/each}
				<tr>
					<td>Checksums</td>
					<td>
						{#if release?.assets[SUMS]}
							<a href={release.assets[SUMS].url}><code>{SUMS}</code></a>
						{:else}
							<code>{SUMS}</code>
						{/if}
					</td>
				</tr>
			</tbody>
		</table>
	</div>
	<p>
		{#if release}
			Other assets, such as the viewer, are on the <a href={release.url}>release's page</a>.
		{:else}
			Releases will be listed on <a href={RELEASES}>GitHub</a>.
		{/if}
	</p>

	<h2 id="build">Build from source</h2>
	<p class="source">
		From the README's <a href="{resolve('/docs')}#build-and-test">Build and test</a>:
	</p>
	{@html data.build}
</div>
