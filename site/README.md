# wirebond's website

A static site in three parts: a landing page (`/`), downloads (`/download`) and the docs (`/docs`
for [README.md](../README.md), `/docs/spec` for [SPEC.md](../SPEC.md)). It is SvelteKit with
`adapter-static`: every page is prerendered to plain HTML and reads with JavaScript off.

```sh
npm ci
npm run check                              # svelte-check: types and Svelte's a11y rules, warnings fail
npm run build                              # writes build/
npm test                                   # Playwright against build/ as it stands: build first
PW_CHROMIUM=/opt/pw-browsers/chromium npm test   # with a Chromium other than Playwright's own
```

Node 22.17 or later. `npm run dev` serves it with reloads while you edit. CI
(`.github/workflows/site.yml`) runs the same steps, once at the root and once with
`BASE_PATH=/wirebond`, the path GitHub Pages serves a project site at; set `BASE_PATH` the same way
to build and test for any host that serves the site below its root. Pages are written as
`download.html` and so on, which most hosts serve at `/download`.

**Publishing.** The site publishes itself to GitHub Pages once the owner turns Pages on: Settings,
Pages, Source: GitHub Actions. From then on every push to `main` that changes the site, README.md
or SPEC.md deploys it to <https://tristancopley.github.io/wirebond/>, and running the `site`
workflow by hand (Actions, site, Run workflow) deploys at once. Until Pages is on, the deploy step
says so in its log and summary and succeeds. A custom domain is the owner's choice to make, since
it costs money; none is set up, and the workflow follows one if it is added in the same settings.

**One source of truth.** The site holds no copy of README.md or SPEC.md. `src/lib/server/markdown.ts`
imports both from the repository root when the site is built and renders them. The landing page
renders README's opening paragraphs (its status line among them), "Modes" and the promise; the
download page renders README's "Install" (installers, requirements, "Without the installer") and
"Build and test" whole. What the pages write themselves is glue: headings, links, the release note,
and the asset table, whose names come from SPEC §22.1. Headings get GitHub's anchors, so `#19-cli`
points at the same section here as on GitHub; links to README.md and SPEC.md stay on the site,
links to any other file go to it on GitHub (at the commit CI builds, `GITHUB_SHA`, else `main`),
and every `§N` becomes a link to that SPEC section. The build fails if any in-page link points at
a heading that does not exist.

**The release.** The download page asks GitHub's API for the latest release while it is built and
says in the build log what it found: a release, none (404), or no answer (no network, a timeout, a
rate limit). It reads the same for the last two, "no release had been published when this page was
built", with the date and a link to GitHub's releases, so the build needs no network. With
`GITHUB_TOKEN` set (CI sets it) the build sends it, since a shared build machine often has used up
GitHub's anonymous limit. In the browser it asks once more, so a release published after the build still
shows; that request to `api.github.com` is the only one any page makes beyond the site itself, and
the other pages ship no JavaScript at all. A content security policy written into each page
enforces that; it allows inline style attributes (`style-src-attr`), which SvelteKit's route
announcer needs, and, by its hash, the site's one stylesheet, which each page carries inline so
that no request holds up its first paint. Nothing else inline. No cookies, no analytics, no fonts
from elsewhere.
Prerendering also writes a `__data.json` beside each page; only the download page reads its own,
and the others are dead weight on the host, accepted rather than restructure the loads.

**Why these choices.** marked renders the Markdown: one small package with no dependencies of its
own, GitHub-flavoured tables built in, and a renderer that is easy to bend (anchors, scrolling code
and tables, `§` links); markdown-it needs plugins for the same, and remark/rehype is a dozen
packages. There is no syntax highlighting: it would add a dependency and the code blocks are short
shell lines. Code blocks and tables scroll sideways inside themselves and carry no `tabindex`: current browsers
let the keyboard scroll them anyway, and a tab stop on every block would cost more than it gives.
There is no theme toggle: the site follows the system's light or dark setting, which
needs no script and no stored choice. The other packages are SvelteKit itself (`@sveltejs/kit`,
its Vite plugin, `adapter-static`, `vite`, `svelte`), TypeScript, `svelte-check`, and
`@playwright/test` for the smoke test, all development dependencies of a site that ships none.
