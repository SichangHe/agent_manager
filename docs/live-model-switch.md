# Live Codex model switch

Use this verified, same-pane procedure when a running Codex session must change models. It is an interactive UI operation, not a helper-script operation. Do not use it on a human-owned `h*` pane unless the human has explicitly authorized that switch.

## Preconditions

- Confirm the exact tmux target is a live Codex pane and capture it first.
- Work only when the pane is waiting for input. Do not interrupt an active `Working` turn merely to switch models.

## Verified picker procedure

1. Send `/model`, then press Enter.
2. Inspect the rendered picker. It may open directly at the model list. If it instead opens the current thinking-effort submenu, press Esc once and inspect again to return to the model list. Confirm that `gpt-5.5` is visibly offered; account availability can vary.
3. Use the visible picker state to select `gpt-5.5`. The picker is color-sensitive, so use its highlighted/selected row rather than assuming a fixed number of Up or Down presses.
4. Press Enter to choose the model.
5. Confirm that the thinking-effort picker appears. Select the desired effort from its visible state and press Enter again.
6. Verify both of these before treating the switch as complete:
   - the transcript says `Model changed to gpt-5.5`; and
   - the Codex footer shows `gpt-5.5` (with the selected effort, when shown).

This is the verified `gpt-5.5` path. For another model, do not assume identical picker order or confirmation text: use only a visibly understood UI flow and fail closed if its model or effort selection cannot be verified.

## Fail closed

Stop without sending more navigation keys if any of these occur:

- the capture is blank or does not clearly show the expected Codex picker;
- the pane is actively `Working` or otherwise has an active turn;
- `/model` does not open a recognizable picker;
- the picker shows a different menu and Esc does not clearly return to a model list;
- the requested model is absent, unavailable, or the result cannot be verified in both transcript and footer.
- selecting the model does not visibly open the expected thinking-effort picker.

Never send blind follow-up keys, rely on a fixed number of arrow presses, or claim success from key submission alone. Preserve the same pane and current model after a failed switch; report the visible state and re-check model availability before any later attempt.
