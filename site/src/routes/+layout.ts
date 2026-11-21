// Every page is prerendered to plain HTML. Only the download page ships JavaScript, to refresh
// the release it was built with; the others need none.
export const prerender = true;
export const csr = false;
