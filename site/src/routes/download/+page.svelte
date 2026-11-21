<script lang="ts">
	import { onMount } from 'svelte';
	import { resolve } from '$app/paths';
	import { latest, PLATFORMS, RELEASES, SUMS, type Release } from '#lib/release.ts';
	import type { PageProps } from './$types';

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
				<p><strong>No release has been published yet.</strong></p>
				<p>
					The one-line installers and the downloads below fetch the latest release from GitHub, so
					they need one to exist. Until then, <a href="#build">build from source</a>.
				</p>
			</div>
		{/if}
	</div>

	<h2>One line</h2>
	<p>On macOS or Linux:</p>
	<!-- svelte-ignore a11y_no_noninteractive_tabindex: a block that scrolls sideways must be reachable from the keyboard -->
	<pre tabindex="0"><code class="language-sh">{data.sh}</code></pre>
	<p>On Windows, in an administrator PowerShell:</p>
	<!-- svelte-ignore a11y_no_noninteractive_tabindex -->
	<pre tabindex="0"><code class="language-powershell">{data.powershell}</code></pre>
	<p>
		Either downloads this machine's executable from the latest release, checks it against the
		release's <code>{SUMS}</code>, and runs <code>wirebond install</code>. Nothing changes on the
		network until you give the device a role; <a href="{resolve('/docs')}#set-up">set it up</a> next.
	</p>

	<h2>Executables</h2>
	<p>
		Without the installer, download your machine's asset and <code>{SUMS}</code>, check the hash, and
		run <code>sudo ./wirebond-&lt;OS&gt;-&lt;arch&gt; install</code> (on Windows,
		<code>.\wirebond-Windows-AMD64.exe install</code> in an administrator PowerShell). The assets are
		named as <code>uname -sm</code> prints the machine.
	</p>
	<!-- svelte-ignore a11y_no_noninteractive_tabindex -->
	<div class="scroll" tabindex="0" role="region" aria-label="Table: executables">
		<table>
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
			These are the names every release carries. Releases will be listed on
			<a href={RELEASES}>GitHub</a>.
		{/if}
	</p>

	<h2 id="build">Build from source</h2>
	<p class="source">
		From the README's <a href="{resolve('/docs')}#build-and-test">Build and test</a>:
	</p>
	{@html data.build}
</div>
