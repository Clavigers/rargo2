#!/usr/bin/env bash
# Install or update the factory on this machine, under /root/factory. Run as
# root from a copy of dev/factory at /root/factory/src. Safe to run again: it
# never touches the log once it exists.
set -euo pipefail
F=/root/factory
cd $F
[ -d venv ] || python3 -m venv venv
venv/bin/pip install -q 'pydantic>=2.9' pytest simpy
[ -d repo ] || git clone -q https://github.com/Clavigers/rargo2.git repo
[ -d plan ] || git clone -q https://github.com/Clavigers/rargo2.git plan
[ -d use ] || git clone -q https://github.com/Clavigers/rargo2.git use
[ -d release ] || git clone -q https://github.com/Clavigers/rargo2.git release
mkdir -p state
(cd src && ../venv/bin/python -m pytest -q -p no:cacheprovider)
if [ ! -s log.jsonl ]; then
  FACTORY_LOG=$F/log.jsonl PYTHONPATH=$F/src venv/bin/python -m factory init src/deploy/config.json
fi
cp src/deploy/factory-*.service /etc/systemd/system/
systemctl daemon-reload
echo "installed; start with: systemctl enable --now factory-observer factory-dictator factory-watchdog factory-viz factory-usage factory-pm factory-power-user"
