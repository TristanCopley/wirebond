#!/bin/sh
# Whether this repository publishes to GitHub Pages, from GitHub's answer to
# GET /repos/{owner}/{repo}/pages: `pages.sh <HTTP status> <response body file>`.
#
# Prints `enabled=true|false` and `base=<path>` lines for $GITHUB_OUTPUT, and on stderr one line a
# person reads in the job's log. A 404 means Pages is off, and that is not a failure: the deploy
# waits until the owner turns it on. Pages on with a branch as its source is reported the same
# way, since only "GitHub Actions" as the source takes this workflow's upload. Any other answer
# is an error, so a broken token or API is not mistaken for "off".
set -eu

status=$1 body=$2

case $status in
404)
	echo "GitHub Pages is off for this repository, so nothing is deployed. To publish the site," \
		"turn it on: Settings, Pages, Source: GitHub Actions." >&2
	echo enabled=false
	;;
200)
	build_type=$(jq -r '.build_type // ""' "$body")
	url=$(jq -r '.html_url // ""' "$body")
	if [ "$build_type" != workflow ]; then
		echo "GitHub Pages is on but publishes from a branch (build_type \"$build_type\"), so this" \
			"workflow deploys nothing. Set Settings, Pages, Source to GitHub Actions." >&2
		echo enabled=false
		exit 0
	fi
	# The site's path on its host: "/wirebond" for https://tristancopley.github.io/wirebond/,
	# empty for a custom domain at its root.
	path=$(printf '%s' "$url" | sed -E 's|^[a-z]+://[^/]+||; s|/+$||')
	case $path in
	'' | /*) ;;
	*)
		echo "GitHub Pages reported a URL this cannot read: \"$url\"" >&2
		exit 1
		;;
	esac
	echo "GitHub Pages is on and serves $url; deploying there." >&2
	echo enabled=true
	echo "base=$path"
	;;
*)
	echo "GitHub answered HTTP $status for the Pages settings:" >&2
	cat "$body" >&2
	exit 1
	;;
esac
