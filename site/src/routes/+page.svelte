<script lang="ts">
	import { resolve } from '$app/paths';
	import type { PageProps } from './$types';

	let { data }: PageProps = $props();
	const spec = resolve('/docs/spec');
</script>

<svelte:head>
	<title>wirebond</title>
	<meta
		name="description"
		content="wirebond lets a struggling device borrow the networks around it, bonding them into one connection that ends at another of your own devices."
	/>
</svelte:head>

<div class="prose">
	<h1>wirebond</h1>
	{@html data.intro}

	<p class="actions">
		<a class="button" href={resolve('/download')}>Download</a>
		<a class="button quiet" href={resolve('/docs')}>Read the docs</a>
	</p>

	<aside class="note" aria-labelledby="status">
		<h2 id="status">Status: release candidate</h2>
		<p>
			wirebond 2.0 is tested in a network simulator and in network namespaces, and has yet to run
			on real hardware.
		</p>
	</aside>

	<h2>Two modes</h2>
	<dl>
		<dt>Performance (the default)</dt>
		<dd>
			Sends every packet over every independent path and keeps the first copy to arrive: the delay
			you see is the best path's at every moment. The price is data.
		</dd>
		<dt>Bandwidth</dt>
		<dd>Spreads packets across the paths for more throughput, with more variation in delay.</dd>
	</dl>

	<h2>What it promises</h2>
	{@html data.promise}

	<h2>What it does not do</h2>
	<ul class="nots">
		<li>
			<strong>It does not beat your direct connection.</strong> wirebond does not try to beat the
			client's own direct connection to the internet.
		</li>
		<li>
			<strong>Leaving at the anchor is a detour.</strong> All of the client's traffic leaves for the
			internet at the anchor, another of your own devices. That detour is a price you accept (<a href="{spec}#1-goals-non-goals-and-the-promise">SPEC §1, N1</a>).
		</li>
		<li>
			<strong>No third-party relay.</strong> Every device in the path is yours: the client, its supports
			and the anchor.
		</li>
		<li>
			<strong>It does not replace your mesh.</strong> Identity, trust, addressing and key transport stay
			with the mesh you already run, such as Tailscale (<a
				href="{spec}#1-goals-non-goals-and-the-promise">SPEC §1, N5</a
			>).
		</li>
		<li>
			<strong>Not yet proven on real hardware.</strong> Everything above has been tested in a simulator
			and in network namespaces only.
		</li>
	</ul>
</div>
