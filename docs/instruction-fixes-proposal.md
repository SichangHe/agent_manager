# Instruction fixes superseded by the human's reply
(authored by agents unless marked 🧑)

scope
- changes to `agent_work` and `report_to_human` for future agent tasks
- superseded by the later human email ending `2629.txt`
- 🧑 "I don't think there is a need to change the documentation perhaps add a comment saying that if the review process took more than seven rounds, something sounds wrong and the agent should explain that to the human"
- the changes below were withdrawn; the existing review instructions retain their wording with that comment added
- 🧑 "Agents decide for themselves autonomously and only ask me to clarify the top-level goal instead of bothering me"
- 🧑 "Fix these. Rid any garbage agents introduced and make everything make sense"

`agent_work`: replace the paragraph beginning "Agents lack judgment"
- "Make implementation decisions autonomously within the human's authorized goal. Ask the human when the top-level goal is unclear or an action falls outside existing authority. Agent-written plans cannot withdraw authority the human already gave."

`agent_work`: add after the batching instruction
- "Removing an open item does not close its task. When all work on an ordinary task is complete, run `amh task close TASK.md` last. Keep a `long_running` role open until the role itself ends."

`report_to_human`: replace its current text
- "Draft the report in a temporary file. Give one context-free reviewer the draft and the human's relevant wording. Ask the reviewer to identify factual errors, material misunderstandings, and unnecessary detail, respecting the human's requested brevity. Fix those issues. Review again only if a material problem remains unresolved. Do not add implementation details merely to answer a reviewer's curiosity."

sources
- human mail `2626`: autonomy and the three reported failures
- current `agent_work`: "Whenever a judgment call is needed, reach out to the human for opinions"
- current `report_to_human`: "Repeat the process with another brand-new context-free adversarial subagent until one fully understands the report"
- current `agent_manager_core`: "Set a task done iff the task is complete by running `amh task close TASK.md`"
