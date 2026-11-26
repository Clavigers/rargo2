#!/bin/bash
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
data=${RARGO_SITE_DATA:-/root/rargo-hq/data}
image=rargo-site:latest
name=rargo-site
port=${RARGO_SITE_PORT:-8086}
docker build -q -t "$image" "$repo/web"
docker rm -f rargo-hq "$name" >/dev/null 2>&1 || true
docker run -d --name "$name" --restart unless-stopped \
    -p "127.0.0.1:$port:80" -v "$data:/data" "$image" >/dev/null
for _ in $(seq 1 20); do
    [ "$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$port/" || true)" = 200 ] && { echo "site: OK (HTTP 200)"; exit 0; }
    sleep 1
done
echo "site did not answer 200: docker logs $name" >&2
exit 1
