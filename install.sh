#!/bin/sh
# Install amh on this machine from the latest GitHub build and write its machine settings; safe to run again (it never overwrites settings).
#   curl -fsSL https://github.com/SichangHe/agent_manager/releases/download/latest/install.sh | sh
# Several hosts at once: scripts/setup-machines. Optional environment: ZULIP_BOT_EMAIL and ZULIP_BOT_API_KEY (this machine's Zulip bot), AMH_WORK_LOGS (default ~/amh_work_logs), AMH_BIN_DIR (default ~/.local/bin).
# Docs: docs/machine-setup.md.
set -eu
repo=SichangHe/agent_manager
case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) target=aarch64-apple-darwin ;;
    Darwin-x86_64) target=x86_64-apple-darwin ;;
    Linux-x86_64) target=x86_64-unknown-linux-gnu ;;
    *) echo "amh install: no build for $(uname -s) $(uname -m); build from source: cargo build --release" >&2; exit 1 ;;
esac
bin_dir=${AMH_BIN_DIR:-$HOME/.local/bin}
mkdir -p "$bin_dir"
curl -fsSL "https://github.com/$repo/releases/download/latest/amh-$target" -o "$bin_dir/amh.new"
chmod 755 "$bin_dir/amh.new"
mv "$bin_dir/amh.new" "$bin_dir/amh"
echo "installed $bin_dir/amh ($target)"

state=${AMH_STATE_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/amh}
mkdir -p "$state"
chmod 700 "$state"
if [ ! -f "$state/zulip.env" ] && [ -n "${ZULIP_BOT_EMAIL:-}" ] && [ -n "${ZULIP_BOT_API_KEY:-}" ]; then
    umask 077
    printf 'ZULIP_SITE=%s\nZULIP_BOT_EMAIL=%s\nZULIP_BOT_API_KEY=%s\n' \
        "${ZULIP_SITE:-https://shagent.zulipchat.com}" "$ZULIP_BOT_EMAIL" "$ZULIP_BOT_API_KEY" > "$state/zulip.env"
    echo "wrote $state/zulip.env"
fi
if [ ! -f "$state/env.toml" ]; then
    work_logs=${AMH_WORK_LOGS:-$HOME/amh_work_logs}
    mkdir -p "$work_logs"
    printf '# This machine only; never committed. Docs: docs/machine-setup.md\nwork_logs = "%s"\n\n[zulip]\nenv_file = "%s/zulip.env"\n' \
        "$work_logs" "$state" > "$state/env.toml"
    chmod 600 "$state/env.toml"
    echo "wrote $state/env.toml"
fi

# The shared settings (agents' channel, main machine) come from the human's public ~/.config repository when this machine has none.
config=${AMH_CFG_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/amh}/config.toml
if [ ! -f "$config" ]; then
    mkdir -p "$(dirname "$config")"
    curl -fsSL https://raw.githubusercontent.com/SichangHe/.config/macos/amh/config.toml -o "$config"
    echo "fetched $config"
fi
[ -f "$state/zulip.env" ] || echo "missing $state/zulip.env: rerun with ZULIP_BOT_EMAIL=... ZULIP_BOT_API_KEY=... for this machine's Zulip bot" >&2
case ":$PATH:" in *":$bin_dir:"*) ;; *) echo "add $bin_dir to PATH" >&2 ;; esac
echo "check: echo hello | amh tell agent main_manager.md --from $(hostname -s)"
