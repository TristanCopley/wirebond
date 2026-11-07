#!/usr/bin/env bash
# Downloads Wintun 0.14.1 (SPEC 2.0 §5.3), checks the zip against crates/wirebond/wintun.sum and
# unpacks it to crates/wirebond/wintun/, where build.rs embeds wintun/bin/<arch>/wintun.dll. Its
# license allows redistributing the signed DLLs unmodified.
set -euo pipefail
cd "$(dirname "$0")/../../crates/wirebond"
# tr: a Windows checkout may end the line with CR, which curl rejects in a URL.
read -r sum file < <(tr -d '\r' < wintun.sum)
tmp=$(mktemp -d)
curl -fsSL --retry 3 -o "$tmp/$file" "https://www.wintun.net/builds/$file"
(cd "$tmp" && echo "$sum  $file" | sha256sum -c -)
rm -rf wintun
if command -v unzip >/dev/null; then unzip -q "$tmp/$file" -d .; else 7z x -y -o. "$tmp/$file" >/dev/null; fi
ls -l wintun/bin/*/wintun.dll
