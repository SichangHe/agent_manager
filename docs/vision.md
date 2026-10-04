# The human's architecture vision for amh

(authored by agents unless marked 🧑; every quoted passage is the human's own words; INFERENCE marks the collecting agent's own reading)

Collected 2026-10-04. `M/N:L` = human email N, line L. Dates come from file mtimes.
Quotes are verbatim from the human (speech-to-text typos kept). Anything not in quotes is my INFERENCE or a plain pointer.
The human never wrote a complete spec of the state model or the blocked-task tracker; gaps are marked.

## 1. State model
- Separate logic from IO, run the logic without IO. M/49:13-16 (2026-07-28): "make only the logic part synchronous and let's also have the logic separate from IO and everything else and make it so that the program program can run in simulation"
- Pure transitions in layers. M/63:4 (2026-07-28): "convert them into well-separated scripts with pure state transitions, window manipulation, and finally the Codex layer"
- Internal model independent of connectors. M/480:21-24 (2026-08-13): "have the internal model of what is going on separate from all the interfacing the tools. For example, we should have internal state representing whether an agent is running or idle. And we should have internal state representing messages sent around and we can test those deterministically regardless of the connectors or emails."
- Why: the old state drifted from reality. M/43:3 (2026-07-28): "The markdown files often do not match the underlying tmux status, and agents frequently do not follow the procedures we want." M/43:7: "I would like state consistency and state stored in SQLite."
- Two layers, control and human. M/44:25-28 (2026-07-28): "maybe we don't only use T mux to get control, but But instead have two layers one layer for control and then the second layer for human editing and interaction. And agents will just use commands to interface with the first layer."
- Fail-safe. M/44:16-19: "if the system we build is buggy, we might be locked out if it is not just a plain TMUX window ... we need some sort of fail safe When the system doesn't really work."
- Authoritative state, first idea. M/574:3 (2026-08-14): "We may also make it so that the event log is the authoritative. State and we only store them on disk and reconstruct current state using them every time we need to. For example, in case of a crash. Of course, this event log needs to be compressed with checkpoints and all e old events still pushed into markdown files."
- Authoritative state, narrowed the same day. M/585:3 (2026-08-14): "Yes, keep doing SQLite for now but also emit events for internal coordination"
  INFERENCE: current rule = SQLite holds state; events are emitted beside it; the pure-log design is deferred, not rejected.
- Markdown is a view, for agents to grep. M/480:11-12 (2026-08-13): "Since we've said the markdown files are read only." M/480:9-10: "agents are good at dealing with file systems and we should just tell them where the markdown files are at."
- Archive. M/480:14-17: "we would need to prune the SQLite database once in a while to stay small and efficient. And I think a sensible limit will be one month. And everything that's pruned should get written to files in directories marked by dates"
- M/570:6 (2026-08-14): "We should auto-delete old resolved records along with their events in the DB but keep them in md files"
- M/587:3 (2026-08-15): "Yes, do rolling 30 day archival. Keep everything displayed in markdown tho, so agents can grep them"
- M/591:9 (2026-08-15): "Don't do base64. Put non-text files in separate files and link to them. No need to truncate everything in a single markdown file neither, only do one file per task and include only human prompts."
- M/591:15: "Keep the md files in a git repo like we do now with work_logs, and automatically periodically commit all changes."
- Simplicity as a constraint on state. M/480:3-5 (2026-08-13): "make simplicity so Small number of layers of redirection and the ease of going in and directly manipulating the underlying constructs when a problem occurs as as underlying principles"
- Specs before code. M/480:26-28: "make sure we make a documentation tree that specifies all the behaviors. When we implement this tool, refer to the file paths in the documentation tree in terms of which English specification is implemented where in the code."
- Human's coding guidance (`getagentsmd get development`): "always use actor model like Erlang GenServer does: isolated actors each owns data exclusively, with sequential data access through messaging the actor." and "always explicitly write out state machines with clear state transitions and actions". Notes review.md: "Are there implicit state or state transitions? Can we convert them to clean state machines?"
- Rust guidance (`getagentsmd get rust_coding`): "Strongly prefer enum over trait." "Use `tokio_gen_server` instead of trait objects as suitable."
- INFERENCE: state = one enum-based state machine per entity (task, agent); `transition(state, event) -> (state, effects)` is pure; the shell does SQLite, sockets, Omnigent.

## 2. Deterministic simulation and testing
- M/49:16-19 (2026-07-28): "I want to use this opportunity to try tiger style. So have uh another agent or maybe chat GPT investigate how we may integrate that as well. Basically I want us to be able to simulate the system running without actually running it and do it deterministically."
- Verification. M/49:9-11 (2026-07-28): "let's make it a verified Rust repository." Reason for sync logic: "verified Rust is not compati compatible with async". Notes: only the human's "assumption-carrying verification" idea (research/static_analysis.md), where "use fuzzing/ simulation tests for confidence" backs assumptions at boundaries.
- M/63:5 (2026-07-28): "Write lots of tests addressing all the use cases we want and the problems we've had."
- M/415:17-19 (2026-08-11): "Have dedicated agents plan this well and then implement it through conduct deterministic or nearly deterministic simulation testing When it is ready switch to this new system"
- M/480:23-24: "we can test those deterministically regardless of the connectors or emails."
- Level to simulate: INFERENCE only. Whole-system runs (agents idle/running, messages, task states, clock) with fake IO. The human wrote "simulate the system running without actually running it". No wording on fault injection or the scheduler.
- Narrowing and reversal, latest wins:
  - M/520:12 (2026-08-14): "We don't need to test killing. Problems will surface when I use it. We need auditability and observability instead so agents can diagnose when bugs do occur"
  - M/661:28 (2026-08-16): "Don't fake stuff, just let me test it"; M/665:3 (08-16): "Skip the faking."
  - M/1200:11 (2026-08-29) agent text promised "real state-transition and contract tests rather than trivial fakes"; the human did not object. Notes: "Trivial mocks and tests covered by static analysis are meaningless!!"
  - M/2470:11-13 (2026-10-04): "Test in production. I don't think there's need to write a bunch of fake tests right now. We'll see if anything goes wrong Give each change a little time to let bugs surface."
  - Verified Rust is not mentioned after July 28 in the AMH threads. INFERENCE: dropped for AMH for now; pure/enum design keeps simulation possible later.

## 3. Concurrency and process model
- M/49:11-13 (2026-07-28): "we should use a actor based model where each process is single threaded but processes talk to each other. I think we do Tokyo Gen server"
- M/415:7-9 (2026-08-11): "we should have one daemon listening at a predetermined Unix socket location and any script would talk to this demon to manipulate agent messages and agent spawns and so on."
- M/415:10-11: "move away from sending messages to agents using Tmux and instead have a direct delivery through Omni Agent."
- M/52:4-6 (2026-07-28): "we don't need HTTPS. Why do we need it? Everything is on local."
- Subcommands under the daemon. M/549:3 (2026-08-14): "try to keep most of the functionalities under sub-commands, which the daemon would call. So we don't need to restart the daemon so often when we update the code."
- M/589:3 (2026-08-15), declining a sandboxed executor: "Just execute the amh command like you would for any other command and get the output from stdout in JSON or something. I don't see why we need anything more complicated than that"
- M/2470:3-10 (2026-10-04): "do a port and replace things part by part ... use py03 to call everything and only have the Rust part do command line parsing"
- Daemon after Oct 4: not stated. INFERENCE: plain subcommands first, daemon later if at all.
- M/415:12-13: "delivery should be guaranteed" (see section 6).
- Omnigent versus tmux, in order:
  - M/79:5-6 (2026-07-28): "we should have codecs being managed by Omni Agent and then omni agent can be accessed from within tmux."
  - M/2075:6 (2026-09-24): "make Omnigent the default when launching agents and add `--tmux` for the old tmux"
  - M/2325:4 (2026-09-30): "Use Codex + Tmux for worker agents, Cursor + Omnigent for managers"
  - M/2353:5 (2026-10-01): "Should use Omnigent by default, tmux as an option"

## 4. Tasks
Lifecycle (final):
- M/554:16 (2026-08-14): "'Add' task when agent put them into the queue, which should have a source (exact email/file edit). 'Resolve' task when agent declare them done. Both should notify the human"
- M/559:3-5 (2026-08-14): "there should only be three stages. I don't think we should say resolve and approve. These are ambiguous. Let's use resolution requested and resolved."
- States per the agent's wording the human accepted (M/563): Added, Resolution requested, Resolved (terminal). Human wrote "yes" there.
Creator and owner:
- M/559:6-7: "track the creator of the task, which should be human, if it's from human or a specific agent if that's agent created."
- M/555:3 (2026-08-14): "When a task is created, the agent who created it should automatically become the owner, and if they delegate it, the target agent needs to explicitly transfer task ownership to themselves"
- M/563:14: "yes. Does not matter what the origin is" (the agent that performs Add owns a human task).
- Agent-made tasks. M/554:18: "agents do not report to humans and do not need human approval, but need task creator approval (so we also need to keep task creators alive when tasks are alive)"
- Freeze during review. M/570:4: "Makes sense. This also keeps the owner alive while the creator decides" (owner frozen while resolution is requested; reject returns it to Added).
- Callers are not authenticated. M/570:6: "We also rely on agents to follow instructions to correctly use AMH instead of having to authenticate them. We should inject an env var to every agent session tho, so we know if callers are agents."
- Human reply: "We don't need a format for the email. Agents are free to interpret what the human means"
- Self-close. M/2418:3 (2026-10-03): "Agents should be able to close themselves. Have a command in amh for that"
Blocked tasks and dependencies:
- M/549:3 (2026-08-14): "the task blocking dependency resolution that would correctly notify task owners when all the tasks their task it is blocked on have been done."
- When a dependency counts as done. M/560:11 (2026-08-14), the agent asked whether Resolve or approved-terminal releases dependents: "The latter" (only the terminal Resolved state).
- Who is notified: the owners of blocked tasks, once all blockers are done (M/549:3). No human statement on cycles, on a blocker that is cancelled, or on dependents whose owner is dead.
- INFERENCE: store blocked_on as edges between tasks; on any task becoming Resolved, recompute and message each owner whose edge set became empty; reject cycles at add time.
- Old-system rule for agents with a blocked marker. M/53:3-5 (2026-07-28): "If they have blocked on, they don't receive reminders to do their pending tasks. If they do not, they do." M/532:10 (2026-08-14): "They should not be spammed when blocked"
- Dead blockers. M/2423:7-8 (2026-10-03), about 51 blocked task files, 43 pointing at agents that no longer exist (agent's count): "Mark them closed instead"
- Unblock condition. M/2230:8-10 (2026-09-28): "Maybe just make it clear what the unblocking condition would be. Or maybe let a manager periodically checks what the workers are blocked on, so there's another pair of eyes." Also against evidence fields: "I don't think having evidence for the blocking is a great idea because agents would overcomplicate it."
- Dependency reasoning by the human planner. M/2469:3-6 (2026-10-04): "they should take a look at the tasks from the task files and what's human pending reason about the dependency relationships."
- Validation after edits. M/1720:5 (2026-09-12): "Maybe we should have a validation script that agents should run after editing task file/TODO file?" M/1730:23: "just do 'After editing a task file or TODO.md, pass omo_task_audit.py'"
- Provenance flags. M/1730:12: "Change the flags to --human-authored etc."
- Pending items must be cleared on completion. M/1720:6 (2026-09-12): "the pending task items have accumulated and not gotten cleared out after completion. How? How do we prevent this from happening?"
- M/1730:9: "Long running simply means that the agent will not be closed if they have zero pending item."
- Duplicate owners must be prevented. M/1730:18: "In the future, we will prevent them." (INFERENCE: one owner per task as a state-model invariant.)
- Task tracking as the first deliverable. M/868:3 (2026-08-21): "previously I asked the AMH agent to implement a task tracking system such that both creation and closing tasks send emails."

## 5. Events, audit, observability
- M/570:6 (2026-08-14): "We should also log all the events (resolution requested, etc.) and who did those (agent ID or tmux window)."
- M/520:12 (2026-08-14): "We need auditability and observability instead so agents can diagnose when bugs do occur"
- M/574:3: "The publish sub subscribe paradigm is perfect for this project." "a pre-existing message queue like RabbitMQ ... too heavy and takes too much memory"
- M/585:3: "also emit events for internal coordination"
- Notes (`software_system_design_values`): "Try to have Observability, Debuggability, Transparency, Operability whenever possible." Development guidance: "Avoid brute-force trial and error; increase observability with logs etc."
- Task adds carry a source. M/554:16: "which should have a source (exact email/file edit)"
- Both Add and Resolve notify the human by email. M/554:16.
- Error mail from watchers. M/2023:5 (2026-09-20): "directly email the human from the watcher automatically via scripting and include the relevant info"
- Same-type suppression of alerts (watcher design, not AMH core). M/706:3-8 (2026-08-17): "it should also not be a duplicate suppression but rather be a same type suppression."

## 6. Messaging
- Separate channels. M/415:11-13 (2026-08-11): "human input should be separately handled so that they don't get mixed up with Asian messages and delivery should be guaranteed."
- M/44:14-15 (2026-07-28): "the human input and system injected things should use separate input"
- M/88:17 (2026-07-29): "Agent message wrapper should only come with agent messages, not system or script messages."
- M/1200:3 (2026-08-29): "agents should be able to directly pump in a message to another agent with a different channel and separate from the human's input box."
- Delivery under typing. M/664:3-5 (2026-08-16): "Existing input should be flushed by sending enter. Messages should be delivered to their corresponding agents."
- Addressing. M/676:3 (2026-08-17): "Let's do tags in subjects. Also, Let's have only 1 tag, which is the agent name"; M/723:5: "let's do [main]"; M/723:26: "Just say only matching tags to the AMH for now"
- Human planning agent. M/652:3-4 (2026-08-16): "a dedicated human planning agent that tracks all the tasks blocked on the human and prioritize which ones to let the human do first. This agent should have the human mailbox (or whatever messaging mechanism we will have in the future, behind an adapter)." and "amh can talk JSON to the email thing, which should be a separate program or set of scripts."
- Human manager. M/2469:3 (2026-10-04): "Start a dedicated Opus five point five lo agent that manages the human."
- Human view:
  - M/723:20 (2026-08-17): "The human gets a view of what's happening and can open an editor to prompt"
  - M/708:3 (2026-08-17): "We should still have some sort of window which the human can switch to or open to directly look at the codecs progress like we currently do with tmux windows."
  - M/586:3 (2026-08-14): "Should be append-only. If the edit changed other things, offer to reopen the file and ask for fix"
  - M/1123:13 (2026-08-28): "We need ways to submit prompts."; M/1123:16: "Prune the legacy tools"
  - M/1232:3 (2026-08-30): "I don't want to use a browser give me a terminal option. Ideally, it would be something like I can type a command within tmux and connect to. And then I can disconnect while it still runs."
- Manager reports read like messages. M/2388:3 (2026-10-03): "make the manager reporting sound more like messaging the manager, to discourage agents to dump info to manager."
- Manager of AMH reports straight to the human. M/851:7 (2026-08-21): "this manager is fully responsible for amh, and should directly report to me instead of going through the main manager"

## 7. Boundaries
- In the binary. M/554:10-14 (2026-08-14): "agent control and task tracking in the binary, email watcher separate, yes" / "good" / "yes" (two command families, `amh agent` and `amh task`, per the agent text he accepted).
- Email watcher outside. M/826:18-19 (2026-08-20): "the email watcher should be outside the amh binary." M/549:3: "that should be a separate thing from the control plane"
- Mail compression outside. M/651:4 (2026-08-16): "That should be separate from the amh CLI bc that is the delivery method."
- Mail routing: M/661:8 (2026-08-16): "Change the existing email watcher script so that it first sees whether the email can be routed to AMH and if not falls back to our existing setup."
- Omnigent. M/52:5-6 (2026-07-28): "use Omni Agent to interface codecs instead of treating it as the control plane. It's basically a set of tools." M/520:22 (2026-08-14): "The rewrite should use omnisent as we discussed earlier and use the mechanisms of existing tooling."
- Tmux: the human view only (M/708:3 above); not the delivery path (M/415:10-11).
- Config. M/591:31 (2026-08-15): "draft an explicit TOML config file pointing the hooks to the files ... The config file can use `$AMH_CFG_DIR`". M/1123:9 (2026-08-28): "amh subcommands are configurable in the configuration file."
- Replaceable components rejected. M/591:19 (2026-08-15): "We don't need this bc I don't plan to swap them out."
- Python callers. M/2401:6 (2026-10-03): "Leave temporary dummy commands that simply say use amh instead"
- Help-driven CLI. M/2388:5 (2026-10-03): "Make it s.t. anyone can start from the root --help page and work their way naturally to understand what subcommands and flags they need"
- Paths. M/2470:16-19 (2026-10-04): "do not leak private information such as local paths ... paths, etc., should be either relative to the usual places like dot config local state etc or be Be adjustable from that."

## 8. Conflicts and changes (latest is current)
- Authoritative state: M/574:3 (Aug 14) event log is authoritative; then M/585:3 (Aug 14) "keep doing SQLite for now but also emit events". Current: SQLite + events.
- State storage: M/43:7 (Jul 28) "state stored in SQLite"; M/574 event log; M/585 SQLite again. Current: SQLite.
- Markdown role: M/480:11 (Aug 13) markdown files read only; M/586:3 (Aug 14) edits via editor become append-only messages; M/591:9 (Aug 15) one file per task, human prompts only. Current: read-only views; edits become messages.
- Archive: M/480:15-16 (Aug 13) "one month"; M/587:3 (Aug 15) rolling 30 days; M/591:9 file layout changed. Current: M/587 + M/591.
- Simulation versus no fakes: M/49 and M/415 (Jul 28, Aug 11) deterministic simulation; M/520:12 (Aug 14), M/661:28 (Aug 16), M/2470:11 (Oct 4) test in production. Current: no new fake tests.
- Verified Rust: M/49:10 (Jul 28) vs silence, and M/2470 (Oct 4) incremental port. Current: not required (INFERENCE).
- Rewrite style: M/415:5-9 (Aug 11) full rewrite + daemon + PyO3 call of Python; M/2423:16 (Oct 3) "Let's trim the Python version first"; M/2470 (Oct 4) port part by part, PyO3 first. Current: M/2470.
- Binary scope: M/549:3 (Aug 14) "three tools in a a single binary"; M/554:10 (Aug 14) agent control + task tracking in binary, watcher separate. Current: M/554.
- Task stages: M/549:3 "acceptance"; M/554:16 "Add"/"Resolve"; M/554:18 extra stage; M/559:3 "only be three stages ... resolution requested and resolved". Current: M/559.
- Dependency release: M/560:11 "The latter" (terminal only). No later change.
- Delegation: M/555:3 target must explicitly take ownership; M/563:14 origin does not matter. Consistent.
- Prompt files: M/522:3 (Aug 14) fixed `~/.config/amh/prompts/` flat dir; M/591:31 (Aug 15) paths come from TOML. Current: M/591.
- Human UI: M/723:20 (Aug 17) editor to prompt, no Herdr; M/1123 (Aug 28) read-only human-view plus prompt submission; M/1200 (Aug 29) separate channel; M/1232 (Aug 30) terminal, "I don't want to use a browser". Current: M/1232.
- Delivery mechanism: M/415:10 (Aug 11) leave tmux; M/2325:4 (Sep 30) tmux for workers; M/2353:5 (Oct 1) Omnigent default, tmux optional. Current: M/2353.
- Daemon: M/415:7 (Aug 11) one daemon and Unix socket; M/549:3 (Aug 14) subcommands called by the daemon; M/589:3 (Aug 15) plain exec; M/2470 silent. Current: unclear; inference above.
- Blocked reminders: M/53:3 (Jul 28) blocked agents get no reminders; M/532:10 (Aug 14) same. Consistent.
- Replaceable components: M/591:19 rejected after the agent's proposal. Current: none.

## 9. Parts of the vision and coverage in design-choices.md
- Logic separate from IO, pure transitions, internal model (M/49, M/63, M/480): partly. Only M/49 and M/63 are quoted, in the Rust section; M/480 is missing.
- Two layers, control and human, with a fail-safe (M/44): no.
- Event log or SQLite as authority, events, publish-subscribe (M/574, M/585): partly. M/574 and M/585 are cited, but not the checkpoint or crash idea, and the conflict is not listed.
- SQLite from the start (M/43:7): no.
- Archive and prune to markdown, 30 days, git: yes (M/587, M/591, M/570).
- Markdown read-only, editor diff (M/480, M/586): partly (M/586 only).
- Deterministic simulation and tiger style: partly (M/49 line, M/415; M/480:23 missing).
- Test-in-production reversal: yes.
- Actor model, single-threaded processes, GenServer: partly (M/49 only; notes principles missing).
- Daemon, Unix socket, subcommands called by it, PyO3 port: yes.
- Omnigent versus tmux order and later default: partly (M/52, M/612 yes; Sep 24-Oct 1 messages missing).
- Task lifecycle, creator, owner, delegation, owner kept alive: yes (M/554-570).
- Dependency release rule and notification of owners: partly (M/549 and M/560 cited, no dead-blocker or blocked-marker lines).
- Blocked-marker reminder rules and unblock condition (M/53, M/532, M/2230, M/2423): no.
- Task audit and validation script (M/1720, M/1730): partly (flags and root only).
- Event logging of who acted: yes (M/570:6).
- Alert same-type suppression (M/697, M/706): no.
- Channels separate from human input, guaranteed delivery: yes.
- Tags and routing, fallback: yes.
- Human planning agent and human manager: partly (M/652 yes; M/2469 no).
- Human view, terminal attach: yes.
- AMH manager reports directly to the human (M/851): no.
- Boundaries (watcher, compression outside, no replaceable components): yes (replaceable: only as a conflict item).
- TOML config, subcommands in config, paths, privacy: yes.
- Self-closing agents, dummy old commands, `--help`-driven nested CLI: yes.
- Documentation tree tied to code (M/480) and system design values from notes: no.
