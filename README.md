# amh: agent manager helper

(authored by agents unless marked 🧑)

one program for agents and managers; usage lives in `amh --help`, not here
- 🧑 "do a port and replace things part by part ... at the beginning use py03 to call everything and only have the Rust part do command line parsing ... Test in production"
- Rust (`src/`): parses the command line (clap), then calls the Python through PyO3
    - `src/cli.rs`: the command tree with every help text
    - `src/main.rs`: turns the parsed action into one JSON object and calls `amh.cli.run`; maps the result and `SystemExit` to the exit status
    - `src/bridge.rs`: the built-in Python module `amh_rs`, through which the remaining Python calls the parts already ported
    - ported so far: settings, task files and task list
    - `amh help tmux` prints `python/amh/tmux.md`
- Python (`python/amh/`): everything else, until each part is ported
    - compiled into the binary and imported from memory, so no source path is needed at run time
    - embedded interpreter: the system Python (`.cargo/config.toml`), UTF-8 forced, standard library only
- port order: one part at a time, each left in production for a while before the next
    - planned: settings (TOML) -> task files and task list -> todo/task actions -> agents -> mail -> watcher
    - the human's earlier design choices to follow while porting: `docs/design-choices.md`
- install: `cargo install --path .`
- watcher: `amh manager watch` in the foreground; `amh-watch.service` is the user systemd unit for it
- `use-amh`: stub that old command names link to
- test: `cargo build --release && python3 test_amh.py`; prints only failures
    - re-runs itself inside the built binary (`AMH_RUN_SCRIPT`), so the Rust parts are exercised; command lines are parsed by the binary (`AMH_PARSE_ONLY=1`)
    - runs on a temporary copy of the root's `*.md` with fakes for Omnigent, tmux, IMAP, SMTP; refuses to run on the live root

records, all under the work-log root (`work_logs` in `env.toml`)
- task file `NAME.md`
    - frontmatter: `status` `runat` `managerat` `is_manager` `tool` `pending_task_items`
        - `runat`: agent address, `omnigent://ID` or tmux `SESSION:WINDOW`, or `retired`
        - `status`: `running` `long_running` `blocked` (+ `blocked_on`) `done`
        - item starting `🧑 ` = the human's request
    - body: free text; whole-line `(notes)`; `(pending)` blocks
- `TODO.md`: sections `current:` `human pending:` `low priority:` `previous:`, rows `NAME.md ADDRESS`
- `manager_mail/PREFIX-UID.txt`: stored human email, `Subject:` line then body
- name of a task file without `.md` = tag in email subjects `[tag]`

modules
- settings (`src/settings.rs`, Rust; the Python reads them through the built-in module `amh_rs`)
    - 🧑 "Separate config file, which should live in .config or something and get committed, and env, which should be in local state or something and include things like where the work_logs is"
    - `config.toml` in the config folder (`$AMH_CFG_DIR`, else `$XDG_CONFIG_HOME/amh`, else `~/.config/amh`): same on every machine, committed; optional
        - `[launch.TOOL]` `model`, `effort`, `manager_model`, `manager_effort`
    - `env.toml` in the state folder (`$AMH_STATE_DIR`, else `$XDG_STATE_HOME/amh`, else `~/.local/state/amh`): this machine only, never committed; required
        - `work_logs`, `main_manager`, `[omnigent] url token`, `[mail] agent_address agent_password human_address human_mailbox_config stored_prefix`, `[guest] address name workdir stored_prefix`
    - both files flatten to dotted keys (`mail.agent_address`); `env.toml` wins
    - the state folder also holds what the program records: the lock, the watcher log, and the `amh-*.txt` records
- `config`: the settings as the Python sees them
- task files and task list (`src/taskfile.rs`, Rust): parse and render task files without a YAML library, find `(pending)` blocks, mark items, place a task's row in `TODO.md`, atomic file replace keeping permissions
    - checked against the previous Python on every file of the live root: same parse, render, blocks, and placement
- `taskfile` (Python): the `Task` record, loading and saving, the one lock file that serializes every writer, finding the caller's task
- `agents`: send, status, stop, launch for Omnigent (HTTP) and tmux
    - `own_address`: the caller's Omnigent session variable, else its tmux pane
    - multi-line message to Omnigent is followed by one typed line
        - Claude Code treats multi-line input as pasted text and does not act on it alone
    - tmux commands drop `TMUX` so they reach the user's default server
- `mail`: send on the tag's thread (agent Gmail account), list/trash the human's unread (human mailbox via himalaya config), fetch unread mail from one sender
    - `compose` + `deliver` build and send every outgoing email, to the human and to the guest
- `work`: what each action does; `problems`, `check`, `tree`, `rotate`
    - `rotate` of the main manager rewrites its address in `env.toml`
- `watch`: loop every 5 s
    - mail intake: store, route, append `(pending)` + source line, mark read
        - route: `[tag]` task; `for manager` at body edge -> its manager; done task -> its manager; else main manager
        - stored file exists = already taken in
    - delivery: each `(pending)` block -> the task's agent; then the marker line is deleted, the source line stays
        - a block in a task with no agent (done or retired) goes to that task's manager, else the main manager
        - an idle Omnigent session whose runner is offline is reachable: the next message brings the runner back
        - stored email is inlined verbatim in `<human_instruction>`
        - 🧑 "if I message an agent and somehow they are not whatever state they are, I should get some sort of response"
            - delivery to the agent fails -> the block goes to its manager, else the main manager, and the human is emailed once that it was rerouted
            - nobody reachable -> the human is emailed once; retried after 10 min
            - delivered human email is recorded in `amh-awaiting.txt`; every email an agent sends is recorded in `amh-sent.txt`
            - no email from that task within 15 min -> the agent is reminded; within 30 min -> its manager is told and the human is emailed
            - human notices are sent once per message and outcome, keys in `amh-told-human.txt`
    - nudges: idle agent with open items is reminded at most every 30 min
    - agent problems (`missing`, `error`): each is reported once, naming the task file, tool, address, manager, and the harness error
        - to the task's manager; for `error` also by email to the human on the task's thread
        - reported keys live in `amh-problem-notices.txt`; a problem that goes away is forgotten, so a recurrence is reported again
        - nothing is reported while Omnigent is unreachable
- `guest`: guest mailbox; mail from `AMH_GUEST_ADDRESS` with SPF pass is stored under `guest_NAME_manager_mail/`, a dedicated agent (task `guest_NAME.md`, NAME from `AMH_GUEST_NAME`) is started or reused, and it answers with `amh tell guest`
- `cli`: one handler per action; `run` dispatches the JSON object from the Rust front end

dropped on purpose from the old scripts
- report envelopes/receipts: `tell manager` posts straight to the manager
- recovery modes, v2 task format, audit watcher, mail compression
- `task record`, `mail trash-replaced`: `task give --from-human`, `tell human --replaces`
