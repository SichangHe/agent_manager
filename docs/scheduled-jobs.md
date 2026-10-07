# Scheduled commands and standing agents

(authored by agents unless marked 🧑)

requirement
- 🧑 `claude_support.md`: "allow some sort of scripts or commands that should be run like a cron job, but is tied to AMH server ... some sort of method to define long running agents that are always there"

configuration for review
- optional tables in `config.toml`; machine-specific paths can be overridden in `env.toml`
- `[job.NAME]`
    - `every = "15m"` for a repeating interval; positive whole numbers with `s`, `m`, `h`, or `d`
    - alternatively `at = "04:00"` for a daily local time
    - `run = "COMMAND"` executes through `sh -c`; optional `dir` sets its working directory
    - alternatively `tell = "TASK"` and `text = "MESSAGE"` send to that agent
- `[keep.TASK]`
    - `dir` is the agent's working directory; `prompt` is the path to its goal file
    - optional `tool` selects the launcher; default is the normal agent tool
    - use the task name without `.md`

```toml
[job.cleanup]
every = "1h"
run = "./cleanup.sh"

[job.reminder]
at = "09:00"
tell = "maintenance"
text = "Check the maintenance queue."

[keep.maintenance]
dir = "."
prompt = "maintenance-goal.txt"
```

operation
- `Jobs::tick` runs inside the AMH watcher; jobs stop being scheduled when the watcher stops
    - it reaps command processes, checks schedules, and checks standing agents every minute
    - restarting a missing standing agent requires two consecutive missing checks
- an interval job first runs immediately; a daily job first seen after its time waits until tomorrow
    - schedules run on the next watcher poll after becoming due; missed intervals are not replayed
    - `every` takes precedence over `at`; `tell` takes precedence over `run`
- the state folder holds `amh-jobs.txt` with the last attempt time and appended `job-NAME.log` command output
    - an unsuccessful start also advances the attempt time to avoid retrying every poll
    - the watcher retains attempt times in memory if writing the record fails
    - an active command is not started again in the same watcher process
    - a command can outlive the watcher; a restarted watcher cannot detect that old process
- revived standing tasks have `long_running` status and retain any standing `blocked_on` condition
    - existing manager roles and their reporting manager are retained
- paths resolve from the watcher working directory; shell commands inherit its environment plus `AMH_JOB=NAME`
    - no shell expansion is applied to `dir` or `prompt`
- use one watcher; scheduling is local to its process and does not coordinate multiple watchers
