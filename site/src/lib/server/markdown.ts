// Renders the repository's own README.md and SPEC.md at build time. The site keeps no copy of
// their text: Vite reads the files from the repository root, and every page that quotes them
// renders the current version.
import { Marked, Renderer, type Token, type Tokens } from 'marked';
import { resolve } from '$app/paths';
import readmeSource from '../../../../README.md?raw';
import specSource from '../../../../SPEC.md?raw';

// The build runs in Node; the site carries no Node types, so this is all it reads.
declare const process: { env: Record<string, string | undefined> };

const REPO = 'https://github.com/TristanCopley/wirebond';
// Links to other files on GitHub point at the commit being built when CI names it, else at main.
const REF = process.env.GITHUB_SHA || 'main';

/** A repository file on GitHub, at the revision this site is built from. */
export function blob(file: string): string {
	return `${REPO}/blob/${REF}/${file}`;
}

export type Heading = { depth: number; text: string; id: string };
export type Doc = { title: string; html: string; toc: Heading[]; url: string };

type Name = 'README.md' | 'SPEC.md';
const sources: Record<Name, string> = { 'README.md': readmeSource, 'SPEC.md': specSource };

// Where a repository file lives on the site; anything else links to GitHub.
function page(file: string): string | null {
	if (file === 'README.md') return resolve('/docs');
	if (file === 'SPEC.md') return resolve('/docs/spec');
	return null;
}

const marked = new Marked({ gfm: true });

function escape(text: string): string {
	return text
		.replace(/&(?!#?\w+;)/g, '&amp;')
		.replace(/</g, '&lt;')
		.replace(/>/g, '&gt;')
		.replace(/"/g, '&quot;')
		.replace(/'/g, '&#39;');
}

export function plain(tokens: Token[]): string {
	return tokens
		.map((t) => ('tokens' in t && t.tokens ? plain(t.tokens) : 'text' in t ? t.text : ''))
		.join('');
}

// GitHub's anchors: lowercase, punctuation dropped, spaces to hyphens, repeats numbered. The
// same section has the same fragment here as on GitHub.
function slugger() {
	const seen = new Map<string, number>();
	return (text: string) => {
		const base = text
			.toLowerCase()
			.replace(/&\w+;/g, '')
			.replace(/[^\p{L}\p{N}\s_-]/gu, '')
			.replace(/\s/g, '-');
		const n = seen.get(base) ?? 0;
		seen.set(base, n + 1);
		return n ? `${base}-${n}` : base;
	};
}

type Lexed = { tokens: Token[]; headings: Heading[]; ids: Map<Token, string> };

function lex(source: string): Lexed {
	const tokens = marked.lexer(source);
	const slug = slugger();
	const headings: Heading[] = [];
	const ids = new Map<Token, string>();
	for (const t of tokens) {
		if (t.type !== 'heading') continue;
		const h = t as Tokens.Heading;
		const text = plain(h.tokens).trim();
		const id = slug(text);
		ids.set(t, id);
		headings.push({ depth: h.depth, text, id });
	}
	return { tokens, headings, ids };
}

// SPEC's numbered sections ("11.4 Liveness") by number, for turning "§11.4" into a link.
const refs = new Map<string, string>();
for (const h of lex(specSource).headings) {
	const m = /^(\d+(?:\.\d+)*)\.?\s/.exec(h.text);
	if (m) refs.set(m[1], h.id);
}

function href(target: string): string {
	if (/^[a-z][a-z0-9+.-]*:/i.test(target) || target.startsWith('#')) return target;
	const [path, hash] = target.split('#', 2);
	const file = new URL(path, 'file:///').pathname.slice(1);
	const local = page(file);
	if (local) return hash ? `${local}#${hash}` : local;
	return `${blob(file)}${hash ? `#${hash}` : ''}`;
}

// Links get their site or GitHub address, and every "§N" becomes a link to that SPEC section. The
// links carry the spec page's path even on that page, so a section quoted elsewhere still works.
function rewrite(tokens: Token[]) {
	const spec = page('SPEC.md') ?? '';
	const inLink = new WeakSet<Token>();
	marked.walkTokens(tokens, (t) => {
		if (t.type === 'link') {
			const link = t as Tokens.Link;
			link.href = href(link.href);
			marked.walkTokens(link.tokens, (c) => void inLink.add(c));
			return;
		}
		if (t.type !== 'text' || ('tokens' in t && t.tokens) || inLink.has(t)) return;
		const text = t as Tokens.Text;
		if (!text.text.includes('§')) return;
		const raw = text.escaped ? text.text : escape(text.text);
		text.text = raw.replace(/§(\d+(?:\.\d+)*)/g, (whole, n: string) => {
			const id = refs.get(n);
			return id ? `<a href="${spec}#${id}">${whole}</a>` : whole;
		});
		text.escaped = true;
	});
}

// The heading ids of the document being rendered; set for each render, read by the renderer.
let ids = new Map<Token, string>();
const base = new Renderer();
const rendering = new Marked({
	gfm: true,
	renderer: {
		heading(this: Renderer, token: Tokens.Heading) {
			const id = ids.get(token);
			const inner = this.parser.parseInline(token.tokens);
			return `<h${token.depth}${id ? ` id="${id}"` : ''}>${inner}</h${token.depth}>\n`;
		},
		code(token: Tokens.Code) {
			const lang = (token.lang ?? '').split(/\s/)[0];
			const cls = lang ? ` class="language-${escape(lang)}"` : '';
			const body = token.escaped ? token.text : escape(token.text);
			// No tabindex: browsers let the keyboard scroll an overflowing block without one.
			return `<pre><code${cls}>${body}</code></pre>\n`;
		},
		table(this: Renderer, token: Tokens.Table) {
			return `<div class="scroll">${base.table.call(this, token)}</div>\n`;
		}
	}
});

function html(lexed: Lexed, tokens: Token[]): string {
	rewrite(tokens);
	ids = lexed.ids;
	return rendering.parser(tokens);
}

/** A whole document: its first heading as the title, the rest as HTML, h2 and h3 as contents. */
export function doc(name: Name): Doc {
	const lexed = lex(sources[name]);
	const [first, ...rest] = lexed.tokens.filter((t) => t.type !== 'space');
	const title = first?.type === 'heading' ? plain((first as Tokens.Heading).tokens) : name;
	const body = first?.type === 'heading' ? rest : lexed.tokens;
	return {
		title,
		html: html(lexed, body),
		url: blob(name),
		toc: lexed.headings.filter((h) => h.depth === 2 || h.depth === 3)
	};
}

function between(name: Name, heading: string | null): { lexed: Lexed; tokens: Token[] } {
	const lexed = lex(sources[name]);
	const all = lexed.tokens;
	const start = all.findIndex(
		(t) => t.type === 'heading' && (heading === null || plain((t as Tokens.Heading).tokens) === heading)
	);
	if (start < 0) throw new Error(`${name} has no section "${heading}"`);
	// A section ends at the next heading of its depth or above; the title's ends at the first h2.
	const depth = Math.max((all[start] as Tokens.Heading).depth, 2);
	let end = start + 1;
	while (end < all.length && !(all[end].type === 'heading' && (all[end] as Tokens.Heading).depth <= depth)) end++;
	return { lexed, tokens: all.slice(start + 1, end) };
}

/** A section's body as HTML, without its heading. `null` names the text under the title. */
export function section(name: Name, heading: string | null): string {
	const { lexed, tokens } = between(name, heading);
	return html(lexed, tokens);
}

/** The `n`th block of one type in a section ("paragraph", "list"...), as HTML and plain text. */
export function block(
	name: Name,
	heading: string | null,
	type: Token['type'],
	n = 0
): { html: string; text: string } {
	const { lexed, tokens } = between(name, heading);
	const found = tokens.filter((t) => t.type === type)[n];
	if (!found) throw new Error(`${name}: no ${type} ${n + 1} in "${heading ?? '(title)'}"`);
	const text = 'tokens' in found && found.tokens ? plain(found.tokens) : 'text' in found ? found.text : '';
	return { html: html(lexed, [found]), text };
}
