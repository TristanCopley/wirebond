#!/bin/sh
# Installs wirebond on macOS or Linux:
#
#   curl -fsSL https://raw.githubusercontent.com/TristanCopley/wirebond/main/install.sh | sh
#
# It downloads this machine's executable from the latest GitHub release, or from the release the
# first argument names (`| sh -s -- v2.0.0`), checks it against the release's SHA256SUMS, and runs
# `wirebond install` as root, which puts wirebond in place as a system service. Nothing changes on
# the network until you give the device a role. Running it again updates or mends the install.
# WIREBOND_URL, a folder holding a release's assets and SHA256SUMS, replaces GitHub's (a mirror,
# or a test).
set -eu

main() {
    os=$(uname -s)
    arch=$(uname -m)
    case $os in
    Darwin)
        # A shell under Rosetta reports x86_64 on Apple silicon, where arm64 is the native build.
        if [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
            arch=arm64
        fi
        case $arch in
        arm64 | x86_64) ;;
        *) die "there is no wirebond build for $os $arch" ;;
        esac
        ;;
    Linux)
        case $arch in
        x86_64 | aarch64 | armv7l | armv6l) ;;
        arm64) arch=aarch64 ;;
        armv8l) arch=armv7l ;; # a 32-bit system on a 64-bit ARM processor
        *) die "there is no wirebond build for $os $arch" ;;
        esac
        ;;
    *) die "this installer is for macOS and Linux; on Windows, use install.ps1 (see the README)" ;;
    esac
    asset=wirebond-$os-$arch

    tag=${1:-latest}
    if [ -n "${WIREBOND_URL:-}" ]; then
        url=${WIREBOND_URL%/}
    elif [ "$tag" = latest ]; then
        url=https://github.com/TristanCopley/wirebond/releases/latest/download
    else
        url=https://github.com/TristanCopley/wirebond/releases/download/$tag
    fi

    if [ "$(id -u)" = 0 ]; then
        sudo=
    elif have sudo; then
        sudo=sudo
    elif have doas; then
        sudo=doas
    else
        die "installing a system service needs root: run this as root, or install sudo"
    fi

    if [ "$os" = Linux ]; then
        [ -d /run/systemd/system ] ||
            die "wirebond runs as a systemd service, and systemd is not running here (on WSL, turn it on in /etc/wsl.conf)"
        have ip || warn "wirebond needs ip, from the iproute2 package (iproute on Fedora)"
        have nft || warn "wirebond needs nft, from the nftables package"
    fi

    tmp=$(mktemp -d 2>/dev/null || mktemp -d -t wirebond)
    trap 'rm -rf "$tmp"' EXIT
    trap 'exit 130' INT TERM
    say "downloading $asset ($tag)"
    fetch "$url/$asset" "$tmp/wirebond"
    fetch "$url/SHA256SUMS" "$tmp/SHA256SUMS"
    want=$(awk -v f="$asset" '$2 == f || $2 == "*" f { print $1; exit }' "$tmp/SHA256SUMS")
    [ -n "$want" ] || die "the release's SHA256SUMS does not list $asset"
    got=$(sha256 "$tmp/wirebond")
    [ "$got" = "$want" ] ||
        die "$asset does not match SHA256SUMS (got $got, want $want); nothing was installed. If a release came out just now, run this again."
    chmod 755 "$tmp/wirebond"
    # It runs here: the right build, and a folder that is not mounted noexec.
    "$tmp/wirebond" version >/dev/null 2>&1 ||
        die "the download does not run from $tmp (is it mounted noexec?): set TMPDIR to another folder and run this again"

    [ -z "$sudo" ] || say "installing as root with $sudo"
    $sudo "$tmp/wirebond" install

    # How to run it as root: sudo's own PATH can leave out /usr/local/bin (Fedora, RHEL).
    wb=wirebond
    if [ -n "$sudo" ] && ! $sudo -n sh -c 'command -v wirebond' >/dev/null 2>&1; then
        wb=/usr/local/bin/wirebond
    fi
    run=${sudo:+$sudo }$wb
    cat <<EOF

Next, on the device with the good connection, the anchor:
    $run anchor on
    $run anchor allow <client>
and on the device on the poor connection, the client:
    $run client <anchor>
<client> and <anchor> are the devices' names on your mesh (tailscale status lists them).
wirebond status shows the bond; wirebond help lists the rest.
EOF
    have tailscale || [ -d /Applications/Tailscale.app ] ||
        warn "Tailscale is not installed here: wirebond needs a mesh, such as Tailscale, joining the two devices"
}

say() {
    printf 'wirebond: %s\n' "$*"
}

warn() {
    printf 'wirebond: warning: %s\n' "$*" >&2
}

die() {
    printf 'wirebond: %s\n' "$*" >&2
    exit 1
}

# Whether the command exists, counting the sbin folders that a user's PATH can leave out.
have() {
    command -v "$1" >/dev/null 2>&1 || [ -x "/usr/sbin/$1" ] || [ -x "/sbin/$1" ]
}

# Downloads $1 into the file $2, which the shell writes: a confined curl (a snap) may not see $TMPDIR.
fetch() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --retry 3 "$1" >"$2" || die "could not download $1"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O - "$1" >"$2" || die "could not download $1"
    else
        die "downloading needs curl or wget"
    fi
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        set -- sha256sum "$1"
    elif command -v shasum >/dev/null 2>&1; then
        set -- shasum -a 256 "$1"
    elif command -v openssl >/dev/null 2>&1; then
        set -- openssl dgst -sha256 -r "$1"
    else
        die "checking the download needs sha256sum, shasum or openssl"
    fi
    "$@" | cut -d ' ' -f 1
}

# Everything above only defines; this line runs it, so a download cut short runs nothing.
main "$@"
