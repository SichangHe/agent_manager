Before starting quota-heavy OpenAI/Codex work on this host, such as long-running or multi-agent work, read the Codex quota:

`codex-quota` (`codex-quota --help` explains the signal; `--json` for programs)

Use its signal to decide whether to prioritize important work, pause lower-priority work, or use spare quota before reset.

🤖 Codex now runs through CLIProxyAPI, so the old `curl -sS 'http://127.0.0.1:18181/api/quota/help'` of CliGate no longer sees the traffic and its signal is wrong; do not use it.
