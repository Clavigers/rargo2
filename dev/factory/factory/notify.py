"""Telling the owner about what needs a person: alarms, where the model and
the world disagree. Posts to the ntfy URL in FACTORY_NOTIFY, if set."""
from __future__ import annotations

import os
import urllib.request


def notify(msg: str, title: str = "rargo factory") -> None:
    url = os.environ.get("FACTORY_NOTIFY")
    if not url:
        return
    req = urllib.request.Request(url, data=msg.encode(), headers={"Title": title})
    try:
        urllib.request.urlopen(req, timeout=20).close()
    except OSError:
        pass  # a missed notification must never stop the factory
