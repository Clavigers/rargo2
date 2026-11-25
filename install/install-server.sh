#!/bin/sh
set -eu
version=${1:-latest}
base=${RARGO_DL:-https://rargo.rs/dl}
P=${RARGO_PREFIX:-}
listen=${RARGO_LISTEN:-0.0.0.0:7459}
admin=${RARGO_ADMIN:-admin}
[ "$(id -u)" = 0 ] || { echo "run this as root: rargod mounts a FUSE file system and creates namespaces" >&2; exit 1; }
[ "$(uname -s)-$(uname -m)" = Linux-x86_64 ] || { echo "the prebuilt server is for Linux x86_64 only" >&2; exit 1; }
[ -e /dev/fuse ] || { echo "/dev/fuse is missing: load the fuse module (modprobe fuse)" >&2; exit 1; }
for tool in curl tar sha256sum git cc; do
    command -v "$tool" >/dev/null || { echo "missing: $tool (builds also need a C compiler and linker; on Debian/Ubuntu: apt install build-essential git curl)" >&2; exit 1; }
done
nix=$(command -v nix || true)
if [ -z "$nix" ] && [ -x /nix/var/nix/profiles/default/bin/nix ]; then
    nix=/nix/var/nix/profiles/default/bin/nix
fi
[ -n "$nix" ] && [ -x "${nix%/*}/nix-store" ] || { echo "install Nix with flakes to run rargod" >&2; exit 1; }
nix_bin=${nix%/*}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
if [ "$version" = latest ]; then
    version=$(curl -fsSL "$base/VERSION") || { echo "cannot read $base/VERSION" >&2; exit 1; }
fi
asset=rargo-server-x86_64-unknown-linux-gnu.tar.gz
echo "downloading $base/$version/$asset"
curl -fsSL -o "$tmp/$asset" "$base/$version/$asset"
curl -fsSL -o "$tmp/SHA256SUMS" "$base/$version/SHA256SUMS"
(cd "$tmp" && grep " $asset\$" SHA256SUMS | sha256sum -c - >/dev/null) || { echo "checksum of $asset does not match" >&2; exit 1; }
tar -xzf "$tmp/$asset" -C "$tmp"
real=$("$tmp/rargo-server/rargod" --version | awk '{print $2}') || { echo "rargod does not run here (it needs glibc 2.39 or newer)" >&2; exit 1; }
"$tmp/rargo-server/planner-cargo" --version >/dev/null 2>&1 || { echo "the planner does not run here (it needs OpenSSL 3 and zlib: apt install libssl3 zlib1g)" >&2; exit 1; }
lib=$P/usr/local/lib/rargo
home=$P/var/lib/rargo
state=$home/state
mkdir -p "$lib/$real" "$state"
install -m 755 "$tmp/rargo-server/rargod" "$tmp/rargo-server/rargo-sandbox" "$tmp/rargo-server/planner-cargo" "$lib/$real/"
install -d "$lib/$real/default-flake"
install -m 644 "$tmp/rargo-server/default-flake/flake.nix" "$tmp/rargo-server/default-flake/flake.lock" "$lib/$real/default-flake/"
ln -sfn "$lib/$real" "$lib/current"
export RUSTUP_HOME=$home/rustup CARGO_HOME=$home/cargo-home
if [ ! -x "$CARGO_HOME/bin/rustup" ]; then
    echo "installing rustup into $home"
    curl -fsSL https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal --default-toolchain stable >/dev/null
fi
invite=""
if [ ! -s "$state/tokens.json" ]; then
    invite=$("$lib/current/rargod" token invite --state "$state" --user "$admin" --admin --uses 5 --days 30 2>/dev/null)
fi
jobs=${RARGO_JOBS:+--jobs $RARGO_JOBS}
mem=${RARGO_MEM_GB:+--mem-gb $RARGO_MEM_GB}
public=${RARGO_PUBLIC:+--public $RARGO_PUBLIC}
run="$lib/current/rargod --state $state --planner $lib/current/planner-cargo --listen $listen $public --cargo-home $CARGO_HOME $jobs $mem"
if [ "${RARGO_NO_SERVICE:-}" = 1 ]; then
    echo "installed rargod $real; not started. To run it:"
    echo "  RUSTUP_HOME=$RUSTUP_HOME CARGO_HOME=$CARGO_HOME PATH=$CARGO_HOME/bin:$nix_bin:\$PATH $run"
else
    cat >"$P/etc/systemd/system/rargod.service" <<UNIT
[Unit]
Description=rargo build server (remote cargo)
Documentation=https://rargo.rs
After=network-online.target
Wants=network-online.target

[Service]
Environment=RUSTUP_HOME=$RUSTUP_HOME
Environment=CARGO_HOME=$CARGO_HOME
Environment=PATH=$CARGO_HOME/bin:$nix_bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
ExecStart=$run
Restart=on-failure
RestartSec=2
Delegate=yes
LimitNOFILE=1048576
TasksMax=infinity

[Install]
WantedBy=multi-user.target
UNIT
    systemctl daemon-reload
    systemctl enable rargod.service >/dev/null 2>&1
    systemctl restart rargod.service
    sleep 1
    systemctl is-active --quiet rargod.service || { echo "rargod did not start: journalctl -u rargod" >&2; exit 1; }
    echo "rargod $real is running on $listen (UDP and TCP; open both in your firewall)"
fi
port=${listen##*:}
if [ -n "$invite" ]; then
    echo
    echo "An invitation for the first user ($admin, administrator; 5 uses, 30 days):"
    echo
    echo "    $invite"
    echo
    echo "On the machine you build from, in a cargo project:"
    echo
    echo "    rargo @THIS-MACHINE:$port"
    echo "    rargo auth          # opens a page; enter the invitation there"
    echo "    rargo build"
    echo
fi
echo "More people: \`rargo auth invite --user NAME\` from your machine once you are signed in (tokens, revoke: likewise),"
echo "or $lib/current/rargod token invite --state $state --user NAME here. No restart either way."
