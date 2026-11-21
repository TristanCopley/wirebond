# wirebond's website

A static site in three parts: a landing page (`/`), downloads (`/download`) and the docs (`/docs`
for [README.md](../README.md), `/docs/spec` for [SPEC.md](../SPEC.md)). It is SvelteKit with
`adapter-static`: every page is prerendered to plain HTML and reads with JavaScript off.

```sh
npm ci
npm run check                              # svelte-check: types and Svelte's a11y rules, warnings fail
npm run build                              # writes build/
npm test                                   # Playwright against build/, served by vite preview
PW_CHROMIUM=/path/to/chrome npm test       # with a Chromium other than Playwright's own
```

Node 22.17 or later. `npm run dev` serves it with reloads while you edit. CI
(`.github/workflows/site.yml`) runs the same four steps and deploys nothing; put `build/` on any
static host. Pages are written as `download.html` and so on, which most hosts serve at `/download`.

**One source of truth.** The site holds no copy of README.md or SPEC.md. `src/lib/server/markdown.ts`
imports both from the repository root when the site is built and renders them; the landing page
quotes README's opening and its promise, and the download page takes the install commands and
"Build and test" from README the same way. Headings get GitHub's anchors, so `#19-cli` points at
the same section here as on GitHub; links to README.md and SPEC.md stay on the site, links to any
other file go to it on GitHub, and every `§N` becomes a link to that SPEC section. The build fails
if any in-page link points at a heading that does not exist. Everything else written on the pages
is taken from those two files; the site claims nothing they do not say.

**The release.** The download page asks GitHub's API for the latest release while it is built and
says in the build log what it found: a release, none (404), or no answer (no network, a timeout, a
rate limit). It reads the same for the last two, "no release has been published yet", so the build
needs no network. In the browser it asks once more, so a release published after the build still
shows; that request to `api.github.com` is the only one any page makes beyond the site itself, and
the other pages ship no JavaScript at all. A content security policy written into each page
enforces that. No cookies, no analytics, no fonts from elsewhere.

**Why these choices.** marked renders the Markdown: one small package with no dependencies of its
own, GitHub-flavoured tables built in, and a renderer that is easy to bend (anchors, scrolling code
and tables, `§` links); markdown-it needs plugins for the same, and remark/rehype is a dozen
packages. There is no syntax highlighting: it would add a dependency and the code blocks are short
shell lines. There is no theme toggle: the site follows the system's light or dark setting, which
needs no script and no stored choice. The other packages are SvelteKit itself (`@sveltejs/kit`,
its Vite plugin, `adapter-static`, `vite`, `svelte`), TypeScript, `svelte-check`, and
`@playwright/test` for the smoke test, all development dependencies of a site that ships none.
