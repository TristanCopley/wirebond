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

<!-- The text comes first, so that the keyboard and a screen reader reach it before the contents.
     On narrow screens the contents follow it, with a link to them under the title; on wide
     screens they stand beside it. -->
<div class="doc">
	<article class="prose">
		<h1>{doc.title}</h1>
		<p class="source">
			Rendered from the repository's <a href={doc.url}>{source}</a> when this site was built.
			<a class="to-toc" href="#contents">Contents</a>
		</p>
		{@html doc.html}
	</article>

	<nav class="toc" id="contents" aria-labelledby="contents-title">
		<h2 id="contents-title">Contents</h2>
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
</div>
