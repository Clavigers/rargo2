
from factory import harness
from factory.model import Config, SessionEnd

CONFIG = Config.model_validate_json(open("deploy/config.json").read())
CLAUDE, CODEX, GROK = (CONFIG.type_of(a) for a in ("claude-code-1", "codex-1", "grok-1"))


def test_real_quota_messages():
    # Seen on the factory machine, 2026-11-26.
    codex = ("ERROR: You’ve hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), "
             "visit https://chatgpt.com/settings/usage to purchase more credits or try again at 9:45 AM.")
    grok = ('Error: Internal error: {\n  "message": "API error (status 402 Payment Required): '
            'Grok Build usage balance exhausted",\n  "http_status": 402\n}')
    assert harness.session_end(CODEX, 1, codex) is SessionEnd.quota
    assert harness.session_end(GROK, 1, grok) is SessionEnd.quota


def test_exit_codes():
    assert harness.session_end(CLAUDE, 0, "hit your usage limit") is SessionEnd.finished
    assert harness.session_end(CLAUDE, 1, "panic: something") is SessionEnd.crashed
    assert harness.session_end(CLAUDE, -9, "") is SessionEnd.killed


def test_commands(monkeypatch):
    for name in ("CLAUDE_CODE", "CODEX", "GROK"):
        monkeypatch.setenv(f"FACTORY_{name}", f"/bin/{name.lower()}")
    assert harness.command(CODEX, "P")[0] == ["/bin/codex", "exec", "-m", "gpt-6.2-sol", "--yolo", "P"]
    assert harness.command(GROK, "P")[0] == ["/bin/grok", "-p", "P", "-m", "grok-4.7", "--always-approve"]
    argv, env = harness.command(CLAUDE, "P")
    assert argv[:3] == ["/bin/claude_code", "-p", "P"] and env == {"IS_SANDBOX": "1"}
