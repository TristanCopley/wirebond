<script lang="ts">
	import Doc from '#lib/Doc.svelte';
	import type { PageProps } from './$types';

	let { data }: PageProps = $props();
</script>

<Doc doc={data.doc} source="README.md" label="README" />
