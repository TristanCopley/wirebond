<script lang="ts">
	import type { Doc } from '#lib/server/markdown.ts';

	let { doc, source, label }: { doc: Doc; source: string; label: string } = $props();

	type Entry = { text: string; id: string; children: { text: string; id: string }[] };
	const toc = $derived(
		doc.toc.reduce<Entry[]>((out, h) => {
			if (h.depth === 2 || out.length === 0) out.push({ text: h.text, id: h.id, children: [] });
			else out[out.length - 1].children.push({ text: h.text, id: h.id });
			return out;
		}, [])
	);
</script>

<svelte:head>
	<title>{label} · wirebond</title>
</svelte:head>

<div class="doc">
	<details class="toc">
		<summary>Contents</summary>
		<nav aria-label="Contents">
			<ol>
				{#each toc as entry (entry.id)}
					<li>
						<a href="#{entry.id}">{entry.text}</a>
						{#if entry.children.length}
							<ol>
								{#each entry.children as child (child.id)}
									<li><a href="#{child.id}">{child.text}</a></li>
								{/each}
							</ol>
						{/if}
					</li>
				{/each}
			</ol>
		</nav>
	</details>

	<article class="prose">
		<h1>{doc.title}</h1>
		<p class="source">
			Rendered from <a href="https://github.com/TristanCopley/wirebond/blob/main/{source}">{source}</a>
			when this site was built.
		</p>
		{@html doc.html}
	</article>
</div>
