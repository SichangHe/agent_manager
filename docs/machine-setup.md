# Setting up amh on another machine
(authored by agents unless marked 🧑)

- 🧑 "The AMH config should be in ~/.config, so they should have been shared."
- why and how messages cross machines: `docs/zulip.md` (agents on several machines)
- shared, in git: `~/.config/amh/config.toml` (the human's `.config` repository)
    - channel `zulip.agent_stream`, main machine `zulip.main_bot`, human ID, launch settings
    - scheduled jobs, standing agents, email, and the human's Zulip messages are handled only on the main machine, so the other entries do nothing elsewhere
- per machine, never committed: the state folder (`$XDG_STATE_HOME/amh`, else `~/.local/state/amh`)
    - `zulip.env`: this machine's own Zulip bot (`ZULIP_SITE`, `ZULIP_BOT_EMAIL`, `ZULIP_BOT_API_KEY`), mode 600
        - a new bot: Zulip Settings > Personal > Bots > generic bot, then subscribe it to the agents' channel
    - `env.toml`:
        ```toml
        work_logs = "/ABSOLUTE/PATH/amh_work_logs"   # an empty folder is enough to only send
        [zulip]
        env_file = "/ABSOLUTE/PATH/.local/state/amh/zulip.env"
        # to run agents here too: [omnigent] url, token; and main_manager
        ```
- install: Rust toolchain, then `git clone https://github.com/SichangHe/agent_manager && cd agent_manager && cargo build --release`, and put `target/release/amh` on `PATH`
- check: `echo hello | amh tell agent main_manager.md --from NAME` prints `sent through Zulip`; the main machine's main manager gets it within ~10 s
- sending from a script or an agent amh did not start: `--from NAME` names the sender; a reply to `NAME` is posted to topic `NAME` of the agents' channel
- receiving here needs Omnigent and the watcher (`amh manager watch`); a task started here records `machine: BOT` and takes posts to its tag
