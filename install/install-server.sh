#!/bin/sh
# Installs a rargo build server on this machine (Linux x86_64, glibc 2.39 or
# newer, root, FUSE) and starts it as a systemd service.
#
#   curl -fsSL https://rargo.rs/install-server.sh | sudo sh
#   curl -fsSL https://rargo.rs/install-server.sh | sudo sh -s -- 0.2.1
#
# What it puts where (RARGO_PREFIX is prepended to all of it; default none):
#   /usr/local/lib/rargo/VERSION/    rargod, rargo-sandbox, planner-cargo
#   /usr/local/lib/rargo/current     -> the version that runs
#   /var/lib/rargo/state/            store and cache
#   /var/lib/rargo/{rustup,cargo-home}/   toolchains and crates (beside the
#                                    state, which builds cannot see)
#   /var/lib/rargo/state/tokens.json who may connect (rargod token ...)
#   /etc/systemd/system/rargod.service
# Settings, all optional, from the environment:
#   RARGO_LISTEN (0.0.0.0:7459)  RARGO_JOBS (all cores)  RARGO_MEM_GB
#   RARGO_ADMIN (admin: name of the first user, invited if nobody is yet)
#   RARGO_PUBLIC  HOST:PORT clients are told to connect to, if not the
#                 name they asked
#   RARGO_NO_SERVICE=1  install the files and print the command, start nothing
# Running it again with a newer version upgrades in place; the state and
# the tokens are kept. The server must be reachable on its port over UDP and TCP.
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
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
# "latest" is turned into its number first, so that every file comes from
# an address that never changes what it holds.
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
# Toolchains: rargod uses rustup and installs what a project asks for.
export RUSTUP_HOME=$home/rustup CARGO_HOME=$home/cargo-home
if [ ! -x "$CARGO_HOME/bin/rustup" ]; then
    echo "installing rustup into $home"
    curl -fsSL https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal --default-toolchain stable >/dev/null
fi
# Who may connect: the holders of tokens, which people get through an
# invitation (`rargo auth`). The first user is invited here.
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
    echo "  RUSTUP_HOME=$RUSTUP_HOME CARGO_HOME=$CARGO_HOME PATH=$CARGO_HOME/bin:\$PATH $run"
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
Environment=PATH=$CARGO_HOME/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
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
