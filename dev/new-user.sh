#!/bin/sh
set -eu
host=${1:-rargo.rs}
how=${2:-cargo}
: "${INVITE:?an invitation from the server's administrator (rargod token invite)}"
t0=$(date +%s)
step() { echo; echo "== $* (+$(( $(date +%s) - t0 )) s)"; }
case $how in
cargo)
    step "cargo install rargo"
    cargo install rargo --locked 2>&1 | tail -n 3
    ;;
script)
    step "curl -fsSL https://$host/install.sh | sh"
    curl -fsSL "https://$host/install.sh" | sh
    PATH=$HOME/.cargo/bin:$HOME/.local/bin:$PATH
    ;;
esac
rargo --rargo-version
work=$(mktemp -d)
cd "$work"
step "a new project"
mkdir hello && cd hello
git init -q .
mkdir src
printf '[package]\nname = "hello"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\nitoa = "1"\n' >Cargo.toml
printf 'fn main() {\n    let mut b = itoa::Buffer::new();\n    println!("hello from a remote build: {}", b.format(6 * 7));\n}\n' >src/main.rs
git add -A && git -c user.name=new -c user.email=new@user.invalid commit -qm one
step "rargo @$host"
manifest=$(cat Cargo.toml)
rargo "@$host"
cat config.rargo
[ "$(cat Cargo.toml)" = "$manifest" ] || { echo "rargo @$host changed Cargo.toml"; exit 1; }
[ -z "$(git status --porcelain)" ] || { git status --short; echo "rargo @$host left git status unclean"; exit 1; }
step "rargo auth"
RARGO_NO_BROWSER=1 rargo auth >"$work/auth.out" 2>&1 &
auth=$!
url=""
for _ in $(seq 1 100); do
    url=$(grep -E '^ +https?://' "$work/auth.out" 2>/dev/null | tr -d ' ' || true)
    [ -n "$url" ] && break
    sleep 0.2
done
[ -n "$url" ] || { cat "$work/auth.out"; echo "rargo auth showed no address"; exit 1; }
code=${url##*code=}
echo "the page: $url"
curl -fsS "$url" >/dev/null
curl -fsS --data-urlencode "code=$code" --data-urlencode "invite=$INVITE" "${url%%/auth*}/auth/approve" | grep -o 'holds a token for <b>[^<]*</b>'
wait $auth
tail -n 1 "$work/auth.out"
rargo auth status
step "rargo build"
rargo build
out=$(./target/debug/hello)
echo "$out"
[ "$out" = "hello from a remote build: 42" ]
[ -z "$(git status --porcelain)" ] || { git status --short; echo "git status is not clean after the build"; exit 1; }
step "rargo test, rargo run"
rargo test 2>&1 | tail -n 2
rargo run -q
rargo --rargo-daemon-stop || true
echo
echo "NEW-USER-PATH-OK in $(( $(date +%s) - t0 )) s"
