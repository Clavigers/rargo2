#!/bin/bash
set -u
want=${1:?usage: deploy/wait-service.sh X.Y.Z}
: "${RARGO_SERVER:?}"
url="https://$RARGO_SERVER/.well-known/rargo.json"
limit=${RARGO_SERVICE_WAIT:-60}
deadline=$(( $(date +%s) + limit ))
seen=""
while :; do
    body=$(curl -s --max-time 5 -w '\n%{http_code}' "$url" 2>&1)
    code=${body##*$'\n'}
    body=${body%$'\n'*}
    got=$(sed -n 's/.*"version":"\([^"]*\)".*/\1/p' <<<"$body")
    if [ "$code" = 200 ] && [ "$got" = "$want" ]; then
        echo "$url answers version $want"
        exit 0
    fi
    seen="status $code${got:+, version $got}"
    if [ "$(date +%s)" -ge "$deadline" ]; then
        echo "deploy/wait-service.sh: waited ${limit} s for $url to answer 200 with version $want; it last gave $seen" >&2
        exit 1
    fi
    sleep 1
done
