# Turn replies
(authored by agents unless marked 🧑)

- 🧑 "Agents really love using the terminal printout for reply, so let's automatically send whatever the agent said at the end of the turn. This also needs to be worked on s.t. pending item notifications and messages replacements work"
- 🧑 redesign: "get rid of the. Rule that does not send. The final. Agent output after. They call AMH message"; "Have their end-of-turn message sent. If they haven't responded. Or had a end of turn, since. Last time the human. Ask them something. Counting the interruption from other messages, as not a end of turn."; "Messages that agents received from the system or other agents, should not be sent back"
- code: `src/replies.rs`; agent-facing text: `work::TURN_REPLY` (first prompt), `cli.rs` agent summary

what
- a human message delivered by the watcher makes a reply owed to the human
- each owed human message is answered by the agent's first natural end of turn after it
    - natural end = an assistant text the agent stopped on by itself
        - next conversation item is a user message: the agent was idle when it arrived
        - or it is the newest item and the session is `ready`, or `running` with it newest for 180 s
    - a message interrupting a turn arrives after a tool result, so the text before it is no end
    - distinct answers go out oldest first, joined; system- or agent-started turns after the answer are not sent
- `amh tell human` does not cancel it; every `amh tell` that sends prints `Never repeat this in your final text.`
- nothing is ever sent back to agents or the system; agents answer agents with `amh tell`
- not allowed to write to the human (`human.writers`) → its manager gets it, clipped, labeled `TASK (end of turn):`

how
- record `amh-replies/SESSION_ID` in the state dir: `human T1 T2 ...`, unanswered human message times
    - time taken before sending; appended after it succeeds; times over 12 h old dropped
- watcher poll `turn replies`: reads newest 500 items; sends once the latest time has an answer
    - file removed after sending only if unchanged meanwhile
    - failed send retried until 10 min after the latest human message; no answer within 12 h → dropped
- human path = `actions::tell_human`: recorded, replaceable with `--replaces`; consolidation reminder, when any, goes to the agent

why the 180 s rule stays
- Claude sessions stay `running` after a turn ends while background commands or subagents run
- no item marks a turn's end; mid-turn the text is followed by its tool call within seconds
- risk: a text followed by over 180 s of generating the next tool call is taken as the end

limits
- items only carry second timestamps; a natural end in the same second as a human message counts as after it
