#!/bin/bash
set -u
work=${RARGO_WORK:-/root/rargo-work}
here=$(cd "$(dirname "$0")" && pwd)
B=${RARGO_BIN:-$(cd "$here/.." && pwd)/target/release}
[ -f "$work/env.sh" ] && . "$work/env.sh"
planner=${PLANNER:-$work/cargo/target/release/cargo}
[ -x "$planner" ] || { echo "no planner at $planner (set PLANNER)" >&2; exit 2; }
mkdir -p "$work"
unset RARGO_SERVER RARGO_TOKEN RARGO_STATE RARGO_HIDE
T=${RARGO_E2E_AUTH:-$work/e2e-auth}
ports=2
port=${E2E_AUTH_PORT:-7466}
host=127.0.0.1:$port
pass=0; fail=0
ok() { pass=$((pass + 1)); echo "ok    $1"; }
bad() { fail=$((fail + 1)); echo "FAIL  $1"; shift; printf '%s\n' "$@" | sed 's/^/        /'; }
check() { local rc=$1 name=$2; shift 2; if [ "$rc" = 0 ]; then ok "$name"; else bad "$name" "$@"; fi; }
reap_clients() {
    local root=$1 pid c dpat gpat pids
    if [ -d "$root/run" ]; then
        (RARGO_CACHE_DIR=$root "$R" --rargo-daemon-stop) || true
    fi
    for c in "$root"/cache*; do
        [ -d "$c" ] || continue
        (RARGO_CACHE_DIR=$c "$R" --rargo-daemon-stop) || true
    done
    dpat=$(printf '%s' "$R --rargo-daemon $root/" | sed 's/[][(){}.^$*+?|\\]/\\&/g')
    gpat=$(printf '%s' "unmount-guard: $root/" | sed 's/[][(){}.^$*+?|\\]/\\&/g')
    pids=$(pgrep -f -- "$dpat" || true)
    pkill -KILL -f -- "$dpat" 2>/dev/null || true
    for pid in $pids; do
        tail --pid="$pid" -f /dev/null
    done
    pids=$(pgrep -f -- "^$gpat" || true)
    for pid in $pids; do
        tail --pid="$pid" -f /dev/null
    done
    if pgrep -f -- "$dpat" >/dev/null 2>&1 || pgrep -f -- "^$gpat" >/dev/null 2>&1; then
        return 1
    fi
    return 0
}

R=$B/rargo
D="$B/rargod"
reap_clients "$T" || { echo "daemons under $T are still running" >&2; exit 2; }
umount -l "$T/proj/target" "$T/proj2/target" 2>/dev/null
pkill -KILL -f "rargod --state $T/state " 2>/dev/null
pkill -KILL -f "fake-github.py [0-9]+ $T/github-who" 2>/dev/null
rm -rf "$T"; mkdir -p "$T/state" "$T/home" "$T/proj/src"
export HOME=$T/home XDG_CONFIG_HOME=$T/home/.config RARGO_CACHE_DIR=$T/cache RARGO_NO_BROWSER=1

echo "== an administrator, before the server runs"
out=$("$B/rargod" --state "$T/state" --planner "$planner" --listen 0.0.0.0:$port --jobs 2 2>&1)
grep -q "nobody could connect" <<<"$out"; check $? "a public server without tokens refuses to start" "$out"
admin=$("$D" token create --state "$T/state" --user admin --admin 2>/dev/null)
[[ "$admin" == t-*.* ]]; check $? "rargod token create makes the administrator's own token, on the server's machine" "$admin"
[ "$(stat -c %a "$T/state/tokens.json")" = 600 ]; check $? "the token file is readable by its owner only"

ghport=$((port + 1))
echo mallory >"$T/github-who"
python3 "$here/fake-github.py" $ghport "$T/github-who" &
gh=$!
export RARGO_GITHUB_URL=http://127.0.0.1:$ghport RARGO_GITHUB_API=http://127.0.0.1:$ghport
"$D" --state "$T/state" --planner "$planner" --listen $host --jobs 4 --mem-gb 8 >"$T/srv.log" 2>&1 &
srv=$!
tail -n +1 -f --pid="$srv" "$T/srv.log" | awk '/serving clients/ { found=1; exit } END { exit found ? 0 : 1 }' || true
grep -q "tokens required" "$T/srv.log"; check $? "the server runs with tokens" "$(cat "$T/srv.log")"

echo "== the administrator, from a client"
adm() { (cd "$T/admin" && HOME=$T/admin XDG_CONFIG_HOME=$T/admin/.config RARGO_CACHE_DIR=$T/cache-admin RARGO_SERVER=$host RARGO_TOKEN=$admin "$R" auth "$@"); }
mkdir -p "$T/admin"
invite=$(adm invite --user ann --uses 2 2>"$T/invite.err")
[[ "$invite" =~ ^[A-Z0-9]{4}(-[A-Z0-9]{4}){3}$ ]]; check $? "rargo auth invite prints a code" "$invite" "$(cat "$T/invite.err")"
grep -q "An invitation for ann, good for 2 use(s)" "$T/invite.err"; check $? "and says for whom, and how often" "$(cat "$T/invite.err")"
! grep -q "${invite//-/}" "$T/state/tokens.json"; check $? "the code is not in the token file"

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
manifest=$(sha256sum Cargo.toml)
out=$("$R" build 2>&1); rc=$?
[ $rc != 0 ] && grep -q 'no rargo server is named here: run `rargo @rargo.rs`' <<<"$out"; check $? "a build where nothing names a server says the one command to run" "$out"
out=$("$R" @$host 2>&1); rc=$?
[ $rc = 0 ] && [ "$(cat config.rargo)" = "host = \"$host\"" ]; check $? "rargo @host writes config.rargo, a new file" "$out" "$(cat config.rargo 2>&1)"
[ "$(sha256sum Cargo.toml)" = "$manifest" ]; check $? "and leaves Cargo.toml byte for byte as it was" "$(cat Cargo.toml)"
[ -z "$(git status --porcelain)" ] && grep -qx '/config.rargo' .git/info/exclude && [ ! -e .gitignore ]; check $? "git status is clean: the file is in .git/info/exclude, .gitignore is not made" "$(git status --porcelain)" "$(cat .git/info/exclude)"
grep -q 'rargo auth' <<<"$out"; check $? "and says what comes next" "$out"
out=$("$R" @127.0.0.1:1 2>&1); "$R" @$host >/dev/null 2>&1
[ "$(cat config.rargo)" = "host = \"$host\"" ] && [ "$(grep -c config.rargo .git/info/exclude)" = 1 ]; check $? "a second rargo @host updates the file in place, and excludes it once" "$out" "$(cat config.rargo)" "$(cat .git/info/exclude)"
out=$("$R" build 2>&1); rc=$?
[ $rc != 0 ] && grep -q "not authenticated.*run \`rargo auth\`" <<<"$out"; check $? "a build before rargo auth says to run it" "$out"
out=$("$R" auth status 2>&1); grep -q "not authenticated" <<<"$out"; check $? "rargo auth status says so too" "$out"

"$R" auth >"$T/auth.out" 2>&1 &
auth=$!
tail -n +1 -f --pid="$auth" "$T/auth.out" | awk '
/confirm there/ { c = 1 }
/^ +[A-Z0-9]{4}-[A-Z0-9]{4}$/ { d = 1 }
c && d { found = 1; exit }
END { exit found ? 0 : 1 }
' || true
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
[ "$(stat -c %a "$cred")" = 600 ] && grep -q "^\[\"$host\"\]" "$cred"; check $? "the token is stored for that host, readable by its owner only" "$(ls -la "$cred"; sed 's/^secret = .*/secret = (hidden)/' "$cred")"
grep -q '^id = "t-' "$cred" && grep -q '^secret = "' "$cred" && grep -q '^name = "ann"' "$cred" && ! grep -q '^\(user\|key\) = ' "$cred"
check $? "it is stored as the token's id, secret and holder" "$(sed 's/^secret = .*/secret = (hidden)/' "$cred")"
out=$("$R" auth status 2>&1); grep -q "authenticated as ann" <<<"$out"; check $? "rargo auth status names the user" "$out"
grep -qx "server named by $T/proj/config.rargo" <<<"$out"; check $? "and the file the server came from" "$out"
out=$(RARGO_SERVER=127.0.0.1:1 "$R" auth status 2>&1); grep -qx "server named by RARGO_SERVER" <<<"$out" && grep -q "^127.0.0.1:1 " <<<"$out"; check $? "RARGO_SERVER outranks config.rargo" "$out"
out=$(RARGO_SERVER=127.0.0.1:1 RARGO_NO_DAEMON=1 "$R" build 2>&1); grep -q "cannot reach the rargo server at 127.0.0.1:1" <<<"$out"; check $? "for a build too" "$out"
out=$("$R" build 2>&1); check $? "rargo build works" "$out"
out=$(./target/debug/hello 2>&1); [ "$out" = "built by the path" ]; check $? "and its binary runs" "$out"
grep -q "ann@" "$T/srv.log" || ! grep -q "t-[a-z0-9]*@" "$T/srv.log"; check $? "the server knows the user by name, not by the token's id" "$(tail -n 3 "$T/srv.log")"

echo "== a second project on the same machine needs no second rargo auth"
mkdir -p "$T/proj2/src"; cd "$T/proj2" || exit 2
git init -q .
printf '[workspace]\nmembers = ["."]\n\n[package]\nname = "second"\nversion = "0.1.0"\nedition = "2021"\n' >Cargo.toml
mkdir -p crates/inner/src
printf '[workspace]\nmembers = [".", "crates/inner"]\n\n[package]\nname = "second"\nversion = "0.1.0"\nedition = "2021"\n' >Cargo.toml
printf '[package]\nname = "inner"\nversion = "0.1.0"\nedition = "2021"\n' >crates/inner/Cargo.toml
: >crates/inner/src/lib.rs
printf 'fn main() {\n    let f = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("config.rargo");\n    println!("cargo:rustc-env=SEEN={}", f.exists());\n}\n' >build.rs
printf 'fn main() {\n    println!("second {}", env!("SEEN"));\n}\n' >src/main.rs
git add -A; git -c user.name=t -c user.email=t@t.invalid commit -qm one
manifests=$(cat Cargo.toml crates/inner/Cargo.toml | sha256sum)
out=$(cd crates/inner/src && "$R" @$host 2>&1); [ -f config.rargo ] && [ ! -e crates/inner/config.rargo ]; check $? "from a workspace member's directory, the file goes to the workspace's root" "$out" "$(find . -name config.rargo)"
[ "$(cat Cargo.toml crates/inner/Cargo.toml | sha256sum)" = "$manifests" ] && [ -z "$(git status --porcelain)" ]; check $? "no manifest changes, and git status is clean" "$(git status --porcelain)"
! grep -q "rargo auth" <<<"$out"; check $? "and is not sent to rargo auth again" "$out"
out=$("$R" run -q 2>&1); [ "$out" = "second false" ]; check $? "rargo run works there, and the server never sees config.rargo" "$out" \
    "$(ls -la target target/debug 2>&1 | head -n 20)" "$(grep -c " $PWD/target " /proc/self/mountinfo) mounts at target/" \
    "$("$R" --rargo-daemon-status 2>&1 | tail -n 6)" "$(tail -n 15 "$RARGO_CACHE_DIR/daemon.log" 2>&1)"
cargo metadata --format-version 1 --no-deps >/dev/null 2>"$T/cargo.err"; check $? "stock cargo reads the manifest" "$(cat "$T/cargo.err")"
mkdir -p "$T/plain/src"; cd "$T/plain" || exit 2
printf '[package]\nname = "plain"\nversion = "0.1.0"\nedition = "2021"\n' >Cargo.toml
printf 'fn main() {\n    println!("plain");\n}\n' >src/main.rs
out=$("$R" @$host 2>&1); [ "$(cat config.rargo)" = "host = \"$host\"" ] && [ ! -e .git ]; check $? "a package outside a workspace, not in git, gets config.rargo and nothing else" "$out" "$(ls -A)"
out=$("$R" run -q 2>&1); [ "$out" = "plain" ]; check $? "and builds" "$out"
"$R" --rargo-daemon-stop >/dev/null 2>&1

echo "== the administrator, from a client, while ann builds"
out=$(adm tokens 2>&1); id=$(awk '$2=="ann"{print $1}' <<<"$out" | head -n 1)
[[ "$id" == t-* ]]; check $? "rargo auth tokens shows ann's token" "$out"
grep -q "for ann: 1 use(s) left" <<<"$out"; check $? "and the invitation's remaining use" "$out"
! grep -q -e "${admin#*.}" -e "$invite" <<<"$out"; check $? "and no secret: neither a token's nor an invitation's" "$out"
tok=$(adm create --user bob --days 30 2>/dev/null)
[[ "$tok" == t-*.* ]]; check $? "rargo auth create makes a token for bob" "$tok"
cd "$T/proj" || exit 2
out=$(RARGO_TOKEN=$tok RARGO_CACHE_DIR=$T/cache-bob "$R" build 2>&1); check $? "it works at once (RARGO_TOKEN)" "$out"
RARGO_TOKEN=$tok RARGO_CACHE_DIR=$T/cache-bob "$R" --rargo-daemon-stop >/dev/null 2>&1
out=$("$R" auth invite --user mallory 2>&1); rc=$?
[ $rc != 0 ] && grep -q "user \`ann\` may not administer this server" <<<"$out"; check $? "ann, who is not an administrator, may not make an invitation" "$out"
out=$("$R" auth allow mallory 2>&1); [ $? != 0 ] && grep -q "may not administer" <<<"$out"; check $? "nor put an account on the allow list" "$out"
out=$("$R" auth tokens 2>&1); [ $? = 0 ] && grep -q "^$id " <<<"$out" && ! grep -q -e " bob " -e " admin " -e "invitations not used up" -e "may sign in" <<<"$out"; check $? "she lists her own tokens, and nobody else's" "$out"
out=$("$R" auth revoke bob 2>&1); [ $? != 0 ] && grep -q "only your own" <<<"$out"; check $? "and may not revoke bob's" "$out"
out=$(RARGO_TOKEN=$tok RARGO_CACHE_DIR=$T/cache-bob "$R" build 2>&1); check $? "which still works" "$out"
RARGO_TOKEN=$tok RARGO_CACHE_DIR=$T/cache-bob "$R" --rargo-daemon-stop >/dev/null 2>&1
out=$(adm revoke "$id" 2>&1); grep -q "revoked 1 token" <<<"$out"; check $? "rargo auth revoke, by the administrator, ends ann's token" "$out"
echo "// changed" >>src/main.rs
out=$("$R" build 2>&1); rc=$?
[ $rc != 0 ]; check $? "a build with the revoked token fails, the session it had open included" "$out"
grep -q "does not know this token, or it was revoked or has expired" <<<"$out" && grep -q "run \`rargo auth\`" <<<"$out"; check $? "and says why, and what to do" "$out"
grep -q "no longer valid" "$T/srv.log"; check $? "the server closed the open session" "$(tail -n 5 "$T/srv.log")"
out=$("$R" auth remove 2>&1); check $? "rargo auth remove forgets the token" "$out"
! grep -q "^\[\"$host\"\]" "$cred"; check $? "it is gone from the credentials file"

echo "== rargo auth remove ends the token on the server too"
export HOME=$T/home3 XDG_CONFIG_HOME=$T/home3/.config RARGO_CACHE_DIR=$T/cache3
cred=$XDG_CONFIG_HOME/rargo/credentials.toml
mkdir -p "$HOME"
cd "$T/proj" || exit 2
tok3=$(adm create --user dave 2>/dev/null)
out=$("$R" auth --token "$tok3" 2>&1); check $? "rargo auth --token stores a token an administrator made" "$out"
grep -q "held by dave" <<<"$out"; check $? "and says whose it is" "$out"
out=$("$R" auth status 2>&1); grep -q "authenticated as dave (token ${tok3%%.*})" <<<"$out"; check $? "rargo auth status names its holder, as the server said" "$out"
echo "// dave" >>src/main.rs
out=$("$R" build 2>&1); check $? "and builds with it" "$out"
out=$("$R" auth remove 2>&1); rc=$?
[ $rc = 0 ] && grep -q "revoked the token for $host on the server" <<<"$out"; check $? "rargo auth remove says it revoked the token on the server" "$out"
echo "// dave again" >>src/main.rs
out=$(RARGO_TOKEN=$tok3 RARGO_CACHE_DIR=$T/cache3b "$R" build 2>&1); rc=$?
[ $rc != 0 ] && grep -q "does not know this token, or it was revoked or has expired" <<<"$out"; check $? "a copy of that token, kept aside and presented again, is refused" "$out"
out=$(RARGO_TRANSPORT=tcp RARGO_TOKEN=$tok3 RARGO_CACHE_DIR=$T/cache3c "$R" build 2>&1); rc=$?
[ $rc != 0 ] && grep -q "does not know this token, or it was revoked or has expired" <<<"$out"; check $? "over TCP too, in the same words" "$out"
out=$("$R" auth --token "$tok3" 2>&1); rc=$?
[ $rc != 0 ] && grep -q "does not know this token" <<<"$out" && ! grep -q "^\[\"$host\"\]" "$cred"; check $? "rargo auth --token refuses a revoked token and keeps nothing" "$out" "$(sed 's/^secret = .*/secret = (hidden)/' "$cred" 2>&1)"
out=$("$R" auth --token "${tok3%%.*}.wrong-secret-wrong-secret" 2>&1); [ $? != 0 ] && grep -q "does not know this token" <<<"$out"; check $? "and one with the wrong secret" "$out"
out=$(curl -fsS -H 'Content-Type: application/json' -d "{\"token\":\"$tok\"}" "http://$host/auth/whoami"); grep -q '"user":"bob"' <<<"$out"; check $? "/auth/whoami names the holder of a token that works" "$out"
out=$(curl -fsS -H 'Content-Type: application/json' -d "{\"token\":\"${tok%%.*}\"}" "http://$host/auth/whoami"); grep -q '"user":null' <<<"$out"; check $? "and nobody for a token's id alone" "$out"
out=$(curl -fsS -H 'Content-Type: application/json' -d "{\"token\":\"${tok3%%.*}.wrong-secret-wrong-secret\"}" "http://$host/auth/revoke"); grep -q '"revoked":false' <<<"$out"; check $? "a token cannot be revoked by its id alone" "$out"
out=$("$R" auth remove 2>&1); [ $? != 0 ]; check $? "a second rargo auth remove has nothing to remove" "$out"
for c in "$T/cache3" "$T/cache3b" "$T/cache3c"; do reap_clients "$c" || true; done

echo "== signing in with GitHub (a stand-in for it), with an allow list"
printf 'the-app-secret\n' >"$T/app-secret"
out=$("$D" token github --state "$T/state" --client-id the-app-id --client-secret-file "$T/app-secret" --url "http://$host" 2>&1); check $? "rargod token github sets the application up, without a restart" "$out"
[ "$(stat -c %a "$T/state/github.json")" = 600 ]; check $? "its secret is readable by the owner only"
out=$(adm allow Carol 2>&1); check $? "rargo auth allow puts an account on the allow list" "$out"
out=$(curl -fsS "http://$host/.well-known/rargo.json"); grep -q '"github":true' <<<"$out"; check $? "the server says that it signs in with GitHub" "$out"
export HOME=$T/home2 XDG_CONFIG_HOME=$T/home2/.config RARGO_CACHE_DIR=$T/cache2
mkdir -p "$HOME"
cd "$T/proj" || exit 2
"$R" auth >"$T/auth2.out" 2>&1 &
auth=$!
tail -n +1 -f --pid="$auth" "$T/auth2.out" | awk '
/^ +[A-Z0-9]{4}-[A-Z0-9]{4}$/ { found = 1; exit }
END { exit found ? 0 : 1 }
' || true
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
out=$(adm tokens 2>&1); grep -q "Carol.*github" <<<"$out" && grep -q "may sign in: Carol" <<<"$out"; check $? "rargo auth tokens shows the token and the allow list" "$out"
out=$(adm disallow carol 2>&1); grep -q "revoked 1 token" <<<"$out"; check $? "rargo auth disallow revokes what the account signed in for" "$out"
echo "// again" >>src/main.rs
out=$("$R" build 2>&1); [ $? != 0 ]; check $? "and its builds stop" "$out"

reap_clients "$T"; check $? "the suite's daemons are gone" "$(ps -eo args | grep -F "$T/" | grep -E 'rargo --rargo-daemon|unmount-guard' | grep -v grep || true)"
kill $gh 2>/dev/null
cd /
kill $srv 2>/dev/null; wait $srv 2>/dev/null
[ "$(grep -c panicked "$T/srv.log")" = 0 ]; check $? "the server did not panic" "$(grep panicked "$T/srv.log")"
echo
echo "$pass passed, $fail failed"
[ "$fail" = 0 ]
