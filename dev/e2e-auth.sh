#!/bin/bash
# End-to-end checks of the path a new user takes (D67):
#   rargo @host   ->   rargo auth   ->   rargo build
# against a server that admits tokens only, with nothing edited by hand, and
# of what an administrator does with tokens while the server runs.
#
#   dev/e2e-auth.sh
#
# Runs on the build machine and on a CI runner: RARGO_BIN (rargod, rargo),
# PLANNER (the cargo fork's binary) and RARGO_WORK (scratch directory) say
# where things are.
#
# The browser's part (opening the page, entering the invitation) is played
# with curl. Needs root (rargod) and a free loopback port.
set -u
work=${RARGO_WORK:-/root/rargo-work}
here=$(cd "$(dirname "$0")" && pwd)
B=${RARGO_BIN:-$(cd "$here/.." && pwd)/target/release}
[ -f "$work/env.sh" ] && . "$work/env.sh"
planner=${PLANNER:-$work/cargo/target/release/cargo}
[ -x "$planner" ] || { echo "no planner at $planner (set PLANNER)" >&2; exit 2; }
mkdir -p "$work"
unset RARGO_SERVER RARGO_USER RARGO_KEY RARGO_TOKEN RARGO_KEY_FILE RARGO_STATE RARGO_HIDE
T=$work/e2e-auth
port=${E2E_AUTH_PORT:-7466}
host=127.0.0.1:$port
pass=0; fail=0
ok() { pass=$((pass + 1)); echo "ok    $1"; }
bad() { fail=$((fail + 1)); echo "FAIL  $1"; shift; printf '%s\n' "$@" | sed 's/^/        /'; }
check() { local rc=$1 name=$2; shift 2; if [ "$rc" = 0 ]; then ok "$name"; else bad "$name" "$@"; fi; }

umount -l "$T/proj/target" "$T/proj2/target" 2>/dev/null
pkill -KILL -f "rargod --state $T/state " 2>/dev/null
pkill -KILL -f "fake-github.py $((${E2E_AUTH_PORT:-7466} + 1)) " 2>/dev/null
rm -rf "$T"; mkdir -p "$T/state" "$T/home" "$T/proj/src"
export HOME=$T/home XDG_CONFIG_HOME=$T/home/.config RARGO_CACHE_DIR=$T/cache RARGO_NO_BROWSER=1
R=$B/rargo
D="$B/rargod"

echo "== an administrator, before the server runs"
out=$("$B/rargod" --state "$T/state" --planner "$planner" --listen 0.0.0.0:$port --jobs 2 2>&1)
grep -q "nobody could connect" <<<"$out"; check $? "a public server without keys or tokens refuses to start" "$out"
invite=$("$D" token invite --state "$T/state" --user ann --uses 2 2>/dev/null)
[[ "$invite" =~ ^[A-Z0-9]{4}(-[A-Z0-9]{4}){3}$ ]]; check $? "rargod token invite prints a code" "$invite"
! grep -q "${invite//-/}" "$T/state/tokens.json"; check $? "the code is not in the token file"
[ "$(stat -c %a "$T/state/tokens.json")" = 600 ]; check $? "the token file is readable by its owner only"

# What stands in for GitHub in the sign-in checks further down.
ghport=$((port + 1))
echo mallory >"$T/github-who"
python3 "$here/fake-github.py" $ghport "$T/github-who" &
gh=$!
export RARGO_GITHUB_URL=http://127.0.0.1:$ghport RARGO_GITHUB_API=http://127.0.0.1:$ghport
"$D" --state "$T/state" --planner "$planner" --listen $host --jobs 4 --mem-gb 8 >"$T/srv.log" 2>&1 &
srv=$!
for _ in $(seq 1 100); do grep -q "serving clients" "$T/srv.log" && break; sleep 0.1; done
grep -q "keys or tokens required" "$T/srv.log"; check $? "the server runs with tokens only" "$(cat "$T/srv.log")"

echo "== what the server says over HTTP on its port"
out=$(curl -fsS "http://$host/.well-known/rargo.json")
grep -q "\"server\":\"$host\"" <<<"$out" && grep -q '"auth":true' <<<"$out"; check $? "it names its address and that it issues tokens" "$out"
out=$(curl -s -o /dev/null -w '%{http_code}' "http://$host/nothing"); [ "$out" = 404 ]; check $? "anything else is not found" "$out"

echo "== a new user"
cd "$T/proj" || exit 2
git init -q .
printf '[package]\nname = "hello"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\n' >Cargo.toml
printf 'fn main() {\n    println!("built by the path");\n}\n' >src/main.rs
git add -A; git -c user.name=t -c user.email=t@t.invalid commit -qm one
out=$("$R" @$host 2>&1); rc=$?
[ $rc = 0 ] && grep -q '^\[package.metadata.rargo\]' Cargo.toml && grep -q "^host = \"$host\"" Cargo.toml; check $? "rargo @host writes the host into Cargo.toml" "$out" "$(cat Cargo.toml)"
grep -q 'rargo auth' <<<"$out"; check $? "and says what comes next" "$out"
out=$("$R" build 2>&1); rc=$?
[ $rc != 0 ] && grep -q "not authenticated.*run \`rargo auth\`" <<<"$out"; check $? "a build before rargo auth says to run it" "$out"
out=$("$R" auth status 2>&1); grep -q "not authenticated" <<<"$out"; check $? "rargo auth status says so too" "$out"

"$R" auth >"$T/auth.out" 2>&1 &
auth=$!
for _ in $(seq 1 50); do grep -q "confirm there" "$T/auth.out" 2>/dev/null && grep -q -E '^ +[A-Z0-9]{4}-[A-Z0-9]{4}$' "$T/auth.out" && break; sleep 0.1; done
code=$(grep -E '^ +[A-Z0-9]{4}-[A-Z0-9]{4}$' "$T/auth.out" | tr -d ' ')
url=$(grep -E '^ +http' "$T/auth.out" | tr -d ' ')
[ -n "$code" ] && [ "$url" = "http://$host/auth?code=$code" ]; check $? "rargo auth shows an address and a code" "$(cat "$T/auth.out")"
out=$(curl -fsS "$url"); grep -q "value=\"$code\"" <<<"$out" && grep -q 'name="invite"' <<<"$out"; check $? "the page shows the code and asks for the invitation" "$out"
out=$(curl -s -w '\n%{http_code}' --data-urlencode "code=$code" --data-urlencode "invite=AAAA-AAAA-AAAA-AAAA" "http://$host/auth/approve")
[ "$(tail -n 1 <<<"$out")" = 403 ]; check $? "a wrong invitation is refused" "$out"
kill -0 $auth 2>/dev/null; check $? "and the terminal keeps waiting"
out=$(curl -s -w '\n%{http_code}' --data-urlencode "code=$code" --data-urlencode "invite=$invite" "http://$host/auth/approve")
[ "$(tail -n 1 <<<"$out")" = 200 ] && grep -q "<b>ann</b>" <<<"$out"; check $? "the invitation is accepted for ann" "$out"
wait $auth; rc=$?
[ $rc = 0 ] && grep -q "Authenticated to $host as ann" "$T/auth.out"; check $? "rargo auth ends authenticated" "$(cat "$T/auth.out")"
cred=$XDG_CONFIG_HOME/rargo/credentials.toml
[ "$(stat -c %a "$cred")" = 600 ] && grep -q "^\[\"$host\"\]" "$cred"; check $? "the token is stored for that host, readable by its owner only" "$(ls -la "$cred"; sed 's/^key = .*/key = (hidden)/' "$cred")"
out=$("$R" auth status 2>&1); grep -q "authenticated as ann" <<<"$out"; check $? "rargo auth status names the user" "$out"
out=$("$R" build 2>&1); check $? "rargo build works" "$out"
out=$(./target/debug/hello 2>&1); [ "$out" = "built by the path" ]; check $? "and its binary runs" "$out"
grep -q "ann@" "$T/srv.log" || ! grep -q "t-[a-z0-9]*@" "$T/srv.log"; check $? "the server knows the user by name, not by the token's id" "$(tail -n 3 "$T/srv.log")"

echo "== a second project on the same machine needs no second rargo auth"
mkdir -p "$T/proj2/src"; cd "$T/proj2" || exit 2
git init -q .
printf '[workspace]\nmembers = ["."]\n\n[package]\nname = "second"\nversion = "0.1.0"\nedition = "2021"\n' >Cargo.toml
printf 'fn main() {\n    println!("second");\n}\n' >src/main.rs
git add -A; git -c user.name=t -c user.email=t@t.invalid commit -qm one
out=$("$R" @$host 2>&1); grep -q '^\[workspace.metadata.rargo\]' Cargo.toml; check $? "a workspace gets workspace metadata" "$out" "$(cat Cargo.toml)"
! grep -q "rargo auth" <<<"$out"; check $? "and is not sent to rargo auth again" "$out"
out=$("$R" run -q 2>&1); [ "$out" = "second" ]; check $? "rargo run works there" "$out" \
    "$(ls -la target target/debug 2>&1 | head -n 20)" "$(grep -c " $PWD/target " /proc/self/mountinfo) mounts at target/" \
    "$("$R" --rargo-daemon-status 2>&1 | tail -n 6)" "$(tail -n 15 "$RARGO_CACHE_DIR/daemon.log" 2>&1)"
cargo metadata --format-version 1 --no-deps >/dev/null 2>"$T/cargo.err"; check $? "stock cargo reads the manifest as before" "$(cat "$T/cargo.err")"

echo "== an administrator, while the server runs"
out=$("$D" token list --state "$T/state" 2>&1); id=$(awk '$2=="ann"{print $1}' <<<"$out" | head -n 1)
[[ "$id" == t-* ]]; check $? "rargod token list shows ann's token" "$out"
grep -q "1 use(s) left" <<<"$out"; check $? "and the invitation's remaining use" "$out"
tok=$("$D" token create --state "$T/state" --user bob --days 30 2>/dev/null)
cd "$T/proj" || exit 2
out=$(RARGO_TOKEN=$tok RARGO_CACHE_DIR=$T/cache-bob "$R" build 2>&1); check $? "a token made on the command line works at once (RARGO_TOKEN)" "$out"
RARGO_TOKEN=$tok RARGO_CACHE_DIR=$T/cache-bob "$R" --rargo-daemon-stop >/dev/null 2>&1
out=$("$D" token revoke --state "$T/state" "$id" 2>&1); check $? "rargod token revoke" "$out"
sleep 4
echo "// changed" >>src/main.rs
out=$("$R" build 2>&1); rc=$?
[ $rc != 0 ]; check $? "a build with the revoked token fails, the session it had open included" "$out"
grep -q -i "does not know this user or key\|rargo auth" <<<"$out"; check $? "and says why" "$out"
grep -q "no longer valid" "$T/srv.log"; check $? "the server closed the open session" "$(tail -n 5 "$T/srv.log")"
out=$("$R" auth remove 2>&1); check $? "rargo auth remove forgets the token" "$out"
! grep -q "^\[\"$host\"\]" "$cred"; check $? "it is gone from the credentials file"

echo "== signing in with GitHub (a stand-in for it), with an allow list"
printf 'the-app-secret\n' >"$T/app-secret"
out=$("$D" token github --state "$T/state" --client-id the-app-id --client-secret-file "$T/app-secret" --url "http://$host" 2>&1); check $? "rargod token github sets the application up, without a restart" "$out"
[ "$(stat -c %a "$T/state/github.json")" = 600 ]; check $? "its secret is readable by the owner only"
out=$("$D" token allow --state "$T/state" Carol 2>&1); check $? "rargod token allow puts an account on the allow list" "$out"
out=$(curl -fsS "http://$host/.well-known/rargo.json"); grep -q '"github":true' <<<"$out"; check $? "the server says that it signs in with GitHub" "$out"
export HOME=$T/home2 XDG_CONFIG_HOME=$T/home2/.config RARGO_CACHE_DIR=$T/cache2
mkdir -p "$HOME"
cd "$T/proj" || exit 2
"$R" auth >"$T/auth2.out" 2>&1 &
auth=$!
for _ in $(seq 1 50); do grep -q -E '^ +[A-Z0-9]{4}-[A-Z0-9]{4}$' "$T/auth2.out" 2>/dev/null && break; sleep 0.1; done
code=$(grep -E '^ +[A-Z0-9]{4}-[A-Z0-9]{4}$' "$T/auth2.out" | tr -d ' ')
out=$(curl -fsS "http://$host/auth?code=$code"); grep -q 'action="/auth/github"' <<<"$out" && ! grep -q 'name="invite"' <<<"$out"; check $? "the page offers GitHub and asks for no invitation" "$out"
out=$(curl -s -o /dev/null -w '%{http_code} %{redirect_url}' "http://$host/auth/github?code=$code")
grep -q "^302 http://127.0.0.1:$ghport/login/oauth/authorize?client_id=the-app-id&redirect_uri=http%3A%2F%2F127.0.0.1%3A$port%2Fauth%2Fgithub%2Fcallback&state=" <<<"$out"; check $? "it sends the browser to GitHub with the application's id and the way back" "$out"
out=$(curl -sL -w '\n%{http_code}' "http://$host/auth/github?code=$code")
[ "$(tail -n 1 <<<"$out")" = 403 ] && grep -q "mallory is not on this server.s allow list" <<<"$out"; check $? "an account that is not allowed gets nothing and is told why" "$out"
kill -0 $auth 2>/dev/null; check $? "and the terminal keeps waiting"
out=$(curl -s -w '\n%{http_code}' "http://$host/auth/github/callback?state=made-up&code=ghcode-carol")
[ "$(tail -n 1 <<<"$out")" = 403 ]; check $? "a callback that belongs to no waiting terminal is refused" "$out"
echo carol >"$T/github-who"
out=$(curl -sL -w '\n%{http_code}' "http://$host/auth/github?code=$code")
[ "$(tail -n 1 <<<"$out")" = 200 ] && grep -q "GitHub account <b>Carol</b>" <<<"$out"; check $? "an allowed account is signed in (as the allow list spells it)" "$out"
wait $auth; rc=$?
[ $rc = 0 ] && grep -q "Authenticated to $host as Carol" "$T/auth2.out"; check $? "rargo auth ends authenticated as that account" "$(cat "$T/auth2.out")"
echo "// carol" >>src/main.rs
out=$("$R" build 2>&1); check $? "rargo build works with the token" "$out"
out=$("$D" token list --state "$T/state" 2>&1); grep -q "Carol.*github" <<<"$out" && grep -q "may sign in: Carol" <<<"$out"; check $? "rargod token list shows the token and the allow list" "$out"
out=$("$D" token disallow --state "$T/state" carol 2>&1); grep -q "revoked 1 token" <<<"$out"; check $? "rargod token disallow revokes what the account signed in for" "$out"
sleep 4
echo "// again" >>src/main.rs
out=$("$R" build 2>&1); [ $? != 0 ]; check $? "and its builds stop" "$out"

for c in "$T/cache" "$T/cache-bob" "$T/cache2"; do RARGO_CACHE_DIR=$c "$R" --rargo-daemon-stop >/dev/null 2>&1; done
kill $gh 2>/dev/null
cd /
kill $srv 2>/dev/null; wait $srv 2>/dev/null
[ "$(grep -c panicked "$T/srv.log")" = 0 ]; check $? "the server did not panic" "$(grep panicked "$T/srv.log")"
echo
echo "$pass passed, $fail failed"
[ "$fail" = 0 ]
