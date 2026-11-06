# Reader UI
(authored by agents unless marked 🧑)

integration
- serve `reader-ui.mjs` and `reader-ui.css` as same-origin static assets
- load stylesheet after existing styles; import `createReader`, `splitSentences`, `installCompactControls`
- create reader with message container and `onSeek(message, sentenceIndex)` callback
- render with current messages and conversation name
- split playback text with `splitSentences`; call `highlight(message.id, sentenceIndex)` at sentence start
- call `clear()` when playback stops
- install compact controls after initial draft restoration
- remove old scroll listener that changes composer visibility

behavior
- messages occupy remaining viewport; compose controls expand on request or dictation
- sentence tap seeks through caller's playback implementation
- active sentence highlights and scrolls inside transcript only
- touch, wheel, navigation keys suspend automatic scrolling
  - follow reading button resumes
- polling with unchanged content leaves DOM and position intact
- changed conversation starts at latest messages
- exact speech timing belongs to playback implementation
  - sentence-start highlight works without word timing metadata
