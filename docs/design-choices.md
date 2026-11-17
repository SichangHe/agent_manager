# The human's design choices for amh

(authored by agents unless marked 🧑; every quoted passage is the human's own words)

Source key: `M/N:L` = the human's email number N, line L, in the stored mail of the work-log root. Collected 2026-10-04.

## 1. Configuration (TOML, prompts, scripts, paths)
- M/588:3 (2026-08-15): "We should have a toml file in the config directory that points to all the prompts in files or allow having them inline"
- M/591:31 (2026-08-15): "Let's decide these later. To decide them, draft an explicit TOML config file pointing the hooks to the files. I think we allow file read and also command runs for each hook. No need to hardcode file locations for which prompt to use when, but instead follow this TOML file. Explain what each prompt file map to in terms of the current stuff we are using. The config file can use `$AMH_CFG_DIR` for paths and use relative paths to itself by default."
  (replies to agent's fixed filenames manager.md, worker.md, watcher-*.md ...; human declined hardcoding them)
- M/522:3-4 (2026-08-14): "As a clarification, let's use ~/.config/amh/prompts/ to store all the prompts in a flat dir" / "Also respect XDG etc."
- M/520:14 (2026-08-14): "It should include all the prompts. Although let's make the default prompts simple and refer to dot config to let users customize each prompt and we should move all the prompts f such as manager dot md to those places."
- M/549:3 (2026-08-14): "And we should make other hooks use prompts from dot config."
- M/1123:9 (2026-08-28): "Additional to human view, let's just make it such that amh subcommands are configurable in the configuration file. That is users can come up with their own subcommands as long as they don't conflict with built-in ones."
- M/1123:10: "Walk me through how the config file currently works."
- M/2470:16-24 (2026-10-04, today's request): "Right now a lot of the configurations are hard coded, which is fine, we can change that later. But things like paths, etc., should be either relative to the usual places like dot config local state etc or be Be adjustable from that." ... "I talked about having a T O M L config file and everything being configurable being produced using a script that would be in the config file."
- M/723:11 (2026-08-17): "Should read root from ~/.config/.env or some file directly or env var. It's global, so no reason to pass that in every time"
- M/1730:25 (2026-09-12, about task-audit helper, same principle): "make the `--root` flag optional for all these commands, by reading it from one of the env files in .config containing the work_logs path"
- M/591:15 (2026-08-15): "Keep the md files in a git repo like we do now with work_logs, and automatically periodically commit all changes."
- M/2470:3-15 (2026-10-04) port plan: "start rewriting the amh command into a Rust. The way we shall do that is we shall do a port and replace things part by part. ... Move that old branch to another name and start the main branch clean move all the current Python AMH scripting to there. Then let the current setup use that AMH setup Rust and PyO three and at the beginning use py03 to call everything and only have the Rust part do command line parsing ... we shall gradually replace parts into Rust rewrites ... eventually replace the whole thing with Rust."

## 2. Command line shape
- M/415:14-16 (2026-08-11): "The CLI agents use should be self evident and in that I mean agents should just be able to run the help command and help command for each sub-command and understand which command to use and how to use them. Use CLAP for that."
- M/2388:3-5 (2026-10-03): "Consolidate the manager helpers. Make them into one command `amh` and make the manager reporting sound more like messaging the manager, to discourage agents to dump info to manager." / "Make the subcommands nested and with sane names instead of the omo_nonsense we currently have. Make it s.t. anyone can start from the root --help page and work their way naturally to understand what subcommands and flags they need"
- M/2388:4: "let them rely on `--help` instead of dumping info via file in my notes"
- M/549:3 (2026-08-14): "try to keep most of the functionalities under sub-commands, which the daemon would call. So we don't need to restart the daemon so often when we update the code."
- M/554:10-14 (2026-08-14), approving agent text "one amh binary with two independent command families: amh agent ... and amh task ...; separate email-watcher scripts": "agent control and task tracking in the binary, email watcher separate, yes" / "good" / "yes"
- M/589:3 (2026-08-15): "Just execute the amh command like you would for any other command and get the output from stdout in JSON or something. I don't see why we need anything more complicated than that"
- M/2401:6 (2026-10-03): "Leave temporary dummy commands that simply say use amh instead"
- M/2418:3 (2026-10-03): "Agents should be able to close themselves. Have a command in amh for that"
- M/1123:16: "Prune the legacy tools"  (re amh human-view-tmux)
- M/1730:23-24 (2026-09-12): "Just do 'After editing a task file or TODO.md, pass omo_task_audit.py'" / "That command is on path, right? Then agents will just run its help page and understand how to use it"

## 3. Agent identity and addressing
- M/676:3 (2026-08-17): "I don't like gmail labels. Let's do tags in subjects. Also, Let's have only 1 tag, which is the agent name"
- M/677:3: "By agent name I mean agent ID or something like that"
- M/723:5 (2026-08-17): "Too long and hard to type, let's do [main]" (agent proposed [main-manager])
- M/723:26: "Unnecessary descriptions. Just say only matching tags to the AMH for now" (untagged/unknown mail falls back to the old system)
- M/661:8 (2026-08-16): "Change the existing email watcher script so that it first sees whether the email can be routed to AMH and if not falls back to our existing setup."
- M/570:6 (2026-08-14): "We also rely on agents to follow instructions to correctly use AMH instead of having to authenticate them. We should inject an env var to every agent session tho, so we know if callers are agents. We should also log all the events (resolution requested, etc.) and who did those (agent ID or tmux window)."
- M/591:35 (2026-08-15): "We map Omnigent agent to our internal agent ID ourselves, right?"
- M/914:3 (2026-08-22, question): "Would the human use the agent's name or ID?"
- M/701:3, M/708:3 (2026-08-17): "there should be a main manager anyways and the human should be able to access the main manager even when there are no tasks" / "We should still have some sort of window which the human can switch to or open to directly look at the codecs progress like we currently do with tmux windows."

## 4. Messaging, email, human view
- M/415:12-13 (2026-08-11): "human input should be separately handled so that they don't get mixed up with Asian [agent] messages and delivery should be guaranteed."
- M/1200:3 (2026-08-29): "It's not that the UI needs to wait for the human to type and finish typing. They should be completely separate. That is agents should be able to directly pump in a message to another agent with a different channel and separate from the human's input box."
- M/549:3: "the email watcher ... should be a separate thing from the control plane"; M/826:18-19 : "the email watcher should be outside the amh binary."
- M/652:3-4 (2026-08-16): "There should also be a dedicated human planning agent that tracks all the tasks blocked on the human ... This agent should have the human mailbox (or whatever messaging mechanism we will have in the future, behind an adapter). I guess amh can talk JSON to the email thing, which should be a separate program or set of scripts."
- M/651:4 (2026-08-16): "the email compression mechanism ... should be separate from the amh CLI bc that is the delivery method."
- M/661:24-26: agent said email-chain summaries are a separate tool; human: "Implement them separately"
- M/549:3: "sending emails to the human for marking tasks as done. That should be added in this new system."
- M/554:16: "Both should notify the human (email; we may support other medias in the future)"
- M/570:6: "We don't need a format for the email. Agents are free to interpret what the human means"
- M/723:20 (2026-08-17): "Read my original instructions on what we are implementing!! No Herdr. The human gets a view of what's happening and can open an editor to prompt"
- M/520:22 and 506:3-6 (2026-08-14): "The rewrite should use omnisent [Omnigent] as we discussed earlier and use the mechanisms of existing tooling." / human editor interface "similar to F stab" requested.
- M/586:3 (2026-08-14), editor workflow: "Should be append-only. If the edit changed other things, offer to reopen the file and ask for fix"
- M/1123:13 (2026-08-28): "Why is it not implemented? We need ways to submit prompts."
- M/1232:3 (2026-08-30): "I don't want to use a browser give me a terminal option. Ideally, it would be something like I can type a command within tmux and connect to. And then I can disconnect while it still runs."
- M/891:4-5 (2026-08-22): interactive agent "expected to receive feedback and explain how the Rust rewrite works"

## 5. Task records
- M/549:3: "a task tracking system which would track the task from human request and their acceptance assigned owner and finally their completion" and "task blocking dependency resolution that would correctly notify task owners when all the tasks their task it is blocked on have been done."
- M/554:16-18: "Let's not use acceptance. 'Add' task when agent put them into the queue, which should have a source (exact email/file edit). 'Resolve' task when agent declare them done." / "We should also allow agents to create tasks, for which agents do not report to humans and do not need human approval, but need task creator approval (so we also need to keep task creators alive when tasks are alive)"
- M/559:3-7: "there should only be three stages ... Let's use resolution requested and resolved." / "track the creator of the task, which should be human ... or a specific agent"
- M/560:11 "The latter" (dependencies release only after approval/terminal). M/555:3 and M/563:14,27: creator becomes owner; delegation needs explicit transfer, "yes. Does not matter what the origin is".
- M/570:4: owner frozen during resolution request: "Makes sense. This also keeps the owner alive while the creator decides"
- M/587:3: "Yes, do rolling 30 day archival. Keep everything displayed in markdown tho, so agents can grep them"
- M/591:9: "Don't do base64. Put non-text files in separate files and link to them. ... only do one file per task and include only human prompts."
- M/570:6: "auto-delete old resolved records along with their events in the DB but keep them in md files"
- M/585:3: "Yes, keep doing SQLite for now but also emit events for internal coordination"
- M/574:3: "we basically want a an event-driven architecture ... publish sub subscribe ... the event log is the authoritative. State" (a question to agents; 585 answers it)

## 6. Rust, architecture, crates, testing
- M/49:11-19 (2026-07-28): "let's make it a verified Rust repository. Another thing is we should use a actor based model where each process is single threaded but processes talk to each other. I think we do Tokyo Gen server but verified Rust is not compati compatible with async so that's make only the logic part synchronous and let's also have the logic separate from IO ... make it so that the program can run in simulation. I want to use this opportunity to try tiger style."
- M/52:4-6 (2026-07-28): "we don't need HTTPS ... Everything is on local. We should use Omni Agent to interface codecs instead of treating it as the control plane."
- M/79:5-6: "we should have codecs being managed by Omni Agent and then omni agent can be accessed from within tmux."
- M/63:4-6: "convert them into well-separated scripts with pure state transitions, window manipulation, and finally the Codex layer. Write lots of tests ..."
- M/415:5-19 (2026-08-11): "rewrite everything we have for the manager orchestration in Rust on top of the Omniagent framework to call Python from Rust use PyO three ... one daemon listening at a predetermined Unix socket location and any script would talk to this demon ... move away from sending messages to agents using Tmux and instead have a direct delivery through Omni Agent ... Use CLAP ... deterministic or nearly deterministic simulation testing"
- M/520:12 (2026-08-14): "We don't need to test killing. Problems will surface when I use it. We need auditability and observability instead so agents can diagnose when bugs do occur"
- M/520:22: "we will just run it alongside the current infrastructure to do some tasks"
- M/612:3-6: "OmniGent is open source ... Omnient supports cursor. There's no need to make them separate."
- M/591:19,22: "We don't need this bc I don't plan to swap them out." / "Unnecessary" (replaceable-executable/SHA pinning proposal)
- M/751:3 (2026-08-18): "Make it 0 missing by implementing all the missing ones" (parity)
- M/661:5: "Feature parity means they can do the same thing."
- M/2470:11-13 (2026-10-04): "Test in production. I don't think there's need to write a bunch of fake tests right now. We'll see if anything goes wrong Give each change a little time to let bugs surface."
- M/661:28: "Don't fake stuff, just let me test it"
- M/2470:20-22: design-choice emails "don't read them yourselves if you do let a claude low subagent do it."

## 7. Privacy and paths
- M/2470:16-17: "remember that do not leak private information such as local paths."

## Conflicts
- Testing: M/415:17-19 (Aug 11) "deterministic or nearly deterministic simulation testing" and M/49:16-19 (Jul 28) simulation; vs M/520:12 (Aug 14) "We don't need to test killing" and M/2470:11-12 (Oct 4) "I don't think there's need to write a bunch of fake tests right now." Latest wins.
- Binary scope: M/549:3 (Aug 14 13:29) "three tools in a a single binary ... email watcher could be exclusive from the binary"; M/554:10 (13:39) "agent control and task tracking in the binary, email watcher separate, yes". Latest wins.
- Prompt storage: M/522:3 (Aug 14) fixed "~/.config/amh/prompts/ ... flat dir"; M/591:31 (Aug 15) "No need to hardcode file locations ... follow this TOML file", paths relative to config with `$AMH_CFG_DIR`.
- Rust approach: M/49:10-11 (Jul 28) "verified Rust repository"; M/2470 (Oct 4) incremental PyO3 port, no mention of verification. M/2423:16 (Oct 3) "Let's trim the Python version first" precedes M/2470 (Oct 4) "start rewriting ... into a Rust".
- Human UI: M/723:20 (Aug 17) "No Herdr ... open an editor to prompt"; M/708:3 tmux-like window; M/1232:3 (Aug 30) terminal attach, "I don't want to use a browser".
