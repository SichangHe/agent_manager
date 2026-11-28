/**
 * Hands-free voice control: start word -> INPUT mode (reply dictation) -> command word -> COMMAND mode (a closed list, chainable, never text);
 * end word sends, cancel word discards. Owns the microphone (recorder), the on-device keyword spotter and the switch question.
 * Everything it does to playback, the feed or the composer goes through `ctx`.
 */
import {createRecorder, EARCON_NOTES, EARCON_NOTE_S} from './recorder.mjs';
import {createKeywordSpotter} from './kws.mjs';
import {assembleReply, looksLikeClippedEndWord, soundsLike, removeControlWords, DOMAIN_PROMPT, DOMAIN_PROMPT_ZH} from './text.mjs';
import {t as tr, getLang} from './i18n.mjs';
import {itemsOf} from './model.mjs';
import {$, toast} from './chrome.mjs';
import {normalize, rankTopics, clearWinner, commandPhrases, afterStartWord as afterStart, stripResumePhrase as stripResume, isCommandWord as isCmd, isCancel, hasCancelWord as hasCancel, endsWithCancel as endsCancel,
  navCommand, choiceAnswer, NEGATIVE_RE, SWITCH_BACK_RE, NEXT_AGENT_RE, SEND_LOG_RE, RESUME_DEFAULT, START_DEFAULT, CMD_DEFAULT, CANCEL_WORD_DEFAULT, LONG_SILENCE_DEFAULT_S} from './grammar.mjs';

const RESUME_SILENCE_MS = 1000;   // plus the recorder's 0.7 s end-of-utterance silence: about 1.7 s of quiet after the phrase
const END_WORD_CHECK_MS = 450, END_WORD_QUIET_MS = 300;   // a local end word sends only if the mic was quiet for the last 300 ms of a 450 ms look-ahead
const FOLLOW_UP_GRACE_MS = 6000;
const EMPTY_SESSION_MS = 8000;
const COMMAND_SILENCE_MS = 8000;
const EARCON_GAIN = {cancel: 0.85, command: 0.75, empty: 0.7};
// 🧑 "play beats once in a while. Maybe every five second ... a double beat. That is low" / "only ... when the user is silent, ... indicating that they are still in either input or command mode"
const MODE_BANNER_MS = 3500;   // how long the "still listening" bar stays after a beat
const MODE_PULSE_MS = 5000;   // a session silent this long gets a low double beat, repeated at this interval; input: low then higher (A3, E4); command: two equal low beats (A3)
EARCON_GAIN.pulseInput = EARCON_GAIN.pulseCommand = 0.35;   // the quiet ones were the ones the human could not hear
const CHOICE_MS = 20000;
const LATE_HIT_WAIT_MS = 600;   // the on-device spotter can report a start word up to ~0.5 s after the clip ended; a clip is never judged keyword-less before that
const EMPTY_GRACE_MS = 3000;
const START_MIN_SPEECH_MS = 250, START_MAX_NO_SPEECH = 0.6, START_MIN_LOGPROB = -1.0;
const PIECE_MAX_CALLS_PER_MIN = 60;   // live refreshes of the growing piece stop when Zulip calls this minute pass this (Zulip allows ~200)
const UPLOADS_PER_MIN = 8, EMPTY_RUN_BACKOFF = 3, BACKOFF_MS = 60000;
/** Mode names as the human reads them (help text and toasts); rename here only. Log keys stay `command mode` etc. */
const COMMAND_SOUND_MS = EARCON_NOTES.command.length * EARCON_NOTE_S * 1000;

/**
 * ctx: {store, log, lat, player, zulip, voice, model, current(), topicOf(key), draft (textarea), autosize(), saveDraft(), dictating(),
 *       maybeAutoplay(), resumeAfterPause(), switchTopic(topic, confirmed), switchBack(), navigate(nav), send(), sendLocal(text, items), onClip, onLevel}
 */
export function createVoiceControl(ctx) {
  const {store, log, lat, player, zulip, voice, model, draft} = ctx;
  const kwsSpotter = createKeywordSpotter({log});

  // ---- settings ----
  if (['over', 'kangaroo', 'rule'].includes(store.get('voice2-resume-phrase', null))) store.set('voice2-resume-phrase', RESUME_DEFAULT);   // the old default migrates; a customised phrase stays
  if (store.get('voice2-start-word', null) === 'pause') store.set('voice2-start-word', START_DEFAULT);   // so does the old start word
  const kwsEnabled = () => store.get('voice2-kws', null) ?? true;   // on-device recognition is the default; the toggle in Settings turns it off
  const startWord = () => normalize(store.get('voice2-start-word', START_DEFAULT));
  const resumePhrase = () => store.get('voice2-resume-phrase', RESUME_DEFAULT);
  const commandWord = () => { const w = normalize(store.get('voice2-cmd-word', CMD_DEFAULT)); return !w || w === 'katana' ? CMD_DEFAULT : w; };   // a saved "katana" is the old default: migrate it
  const cancelWord = () => { const w = normalize(store.get('voice2-cancel-word', CANCEL_WORD_DEFAULT)); return !w || w === 'bamboo' || w === 'macaroni' ? CANCEL_WORD_DEFAULT : w; };   // a saved "bamboo" or "macaroni" is an old default: migrate it
  const afterStartWord = text => afterStart(text, startWord());
  const stripResumePhrase = text => stripResume(text, resumePhrase());
  const isCommandWord = w => isCmd(w, commandWord());
  const CANCEL_RE = {test: t => isCancel(t, cancelWord())};
  const rank = query => rankTopics(model.topics(), query);

  // ---- state ----
  let voiceControl = store.get('voice2-vc', false), voiceListening = false;   // a saved "on" needs one tap after load before the microphone may open
  let vc = null;   // the open session: `{base, text, timer, emptyTimer, mode, parts, openedAt, sawCommand, ending, toldOffline}`
  let pendingChoice = null;   // a switch waiting for yes/no/number: `{names, until, confirm}`
  let utterancePending = false, utteranceTimer = 0;   // set on speech detection, cleared when its clip is handled (or after 6 s if no clip comes)
  let announcingUntil = 0, sayingUntil = 0;   // a switch announcement / our own prompt is playing until then
  let bargePaused = false, lastReplyEndMs = 0, lastVoiceReply = {key: '', ms: 0};
  const kwsHitMs = {};
  let lastHitAt = 0;   // when the latest local keyword fired, to log hit -> feedback
  let lastLocalCommandMs = 0, lastLocalTag = '', lastLocalTagMs = 0, localPhrases = new Map(), lastCommandWindowMs = 0, prebuildTimer = 0;
  const SKIP_COUNT_WAIT_MS = 6000, SKIP_IDLE_MS = 1200;   // 🧑 2026-11-22 "recognize the command quickly. Play an acknowledgement sound ... and then start playing. Instead of flickering around": the sound confirms at once, playback waits for the count (the transcript took ~1.9 s in the log) and moves once   // 🧑 "I don't want the skip command to become slow. Maybe wait for one second"   // how long an on-device "skip" waits for the transcript's count
  let lastLocalNav = null;   // {dir, ms}: a "skip forward/backward" heard on the device; the transcript may add a count
  let startWordUploads = [], emptyRun = 0, backoffUntil = 0;
  let emptyClosedAt = 0;   // when an empty session last timed out: speech that began within EMPTY_GRACE_MS after it was meant for that session

  const choiceOpen = () => !!pendingChoice && Date.now() < pendingChoice.until;
  /** One gate: manual dictation, an open session, an utterance being captured or transcribed, an announcement, an open switch question. No playback starts while it is true. */
  const userSpeaking = () => ctx.dictating() || !!vc || utterancePending || Date.now() < announcingUntil || choiceOpen();
  const utteranceDone = () => { utterancePending = false; clearTimeout(utteranceTimer); if (!userSpeaking()) ctx.maybeAutoplay(); };
  const setDraft = text => { draft.value = text; ctx.autosize(); };
  const joinText = (...xs) => xs.filter(Boolean).join(' ');
  /** The reply text: text appended whole (`fixed`) plus every piece's latest transcript in speaking order. */
  const recompute = () => { vc.text = joinText(vc.fixed, ...[...vc.pieces].sort((a, b) => a[0] - b[0]).map(([, e]) => e.text)); };
  const appendText = t => { vc.fixed = joinText(vc.fixed, t); recompute(); };
  const setPiece = (id, t) => { const e = vc.pieces.get(id); if (e) e.text = t; recompute(); showSessionText(); };
  const showSessionText = () => setDraft([vc.base.trim(), vc.text].filter(Boolean).join(' '));

  // 🧑 "maybe the user would not understand that the beats. Are. Indicating that they are still in a certain mode, so maybe we should also add. Visual indicators in case they freak out."
  // 🧑 "You should. Show the indicator for still listening when you play that beat": the bar appears with each beat for a few seconds, and goes when the session ends.
  let bannerUntil = 0;
  function showModeBanner(flash = false) {
    const el = $('mode-banner'); if (!el) return;
    const mode = vc && !vc.ending ? (vc.mode === 'command' ? 'command' : 'input') : null;
    if (flash) bannerUntil = Date.now() + MODE_BANNER_MS;
    el.hidden = !mode || Date.now() > bannerUntil; if (el.hidden) return;
    el.dataset.mode = mode;
    $('mode-name').textContent = mode === 'command' ? tr('mode.commandTitle') : tr('mode.inputTitle');
    $('mode-hint').textContent = mode === 'command'
      ? tr('mode.commandHint', {resume: resumePhrase(), cancel: cancelWord()})
      : tr('mode.inputHint', {resume: resumePhrase(), cancel: cancelWord()});
  }
  let vcShown = ['off', 'off'];
  const setVcState = (state, key) => { vcShown = [state, key]; showModeBanner(); const label = tr('vc.' + key); $('listen').dataset.state = state; $('listen-label').textContent = label; $('listen').title = tr('vc.aria', {label}); $('listen').setAttribute('aria-label', tr('vc.aria', {label})); };
  const vcIdle = () => { const playing = ['playing', 'loading'].includes(player.position().status); setVcState(playing ? 'listening' : 'on', playing ? 'listening' : 'paused'); };

  const recorder = createRecorder({
    pieceMsPerAudioS: () => voice.sttMsPerAudioS(),
    log, partialReady: () => !pieceBusy && !zulip.rateLimitedForMs() && !zulip.lowBudget() && zulip.callsLastMinute() < PIECE_MAX_CALLS_PER_MIN, onClip: ctx.onClip, onLevel: ctx.onLevel, onBarge, onCommandClip,
    onSpeechOnset: () => { lastVoiceMs = Date.now(); if (!kwsSpotter.ready()) return; for (const w of kwsSpotter.restart(0.6)) onKwsHit(w); if (vc?.mode === 'command' || choiceOpen()) for (const tag of kwsSpotter.restartCommands(0.6)) onCommandHit(tag); },
    onKwsAudio: samples => { for (const w of kwsSpotter.feed(samples)) onKwsHit(w); if (vc?.mode === 'command' || choiceOpen()) for (const tag of kwsSpotter.feedCommands(samples)) onCommandHit(tag); },
    onEarcon: kind => { lastPulseMs = Date.now(); player.earcon(EARCON_NOTES[kind], EARCON_NOTE_S, EARCON_GAIN[kind] ?? 0.45); try { navigator.vibrate?.(kind === 'cancel' ? [60, 40, 60] : 0); } catch { /* unsupported (iOS) */ } if (lastHitAt && Date.now() - lastHitAt < 1500) log('lat', `local hit -> ${kind} sound started in ${Date.now() - lastHitAt}ms`); },
  });

  /** Our own spoken prompt: the microphone ignores speech while it plays; a keyword still interrupts it. */
  function sayAndMute(text) { sayingUntil = Date.now() + 2500 + text.length * 70; recorder.muteFor(2500 + text.length * 70, true); player.say(text).then(ms => { sayingUntil = Date.now() + ms; recorder.muteFor(ms, true); }); }
  function sayThen(text, then) { sayingUntil = Date.now() + 2500 + text.length * 70; recorder.muteFor(2500 + text.length * 70, true); player.say(text).then(ms => { sayingUntil = Date.now() + (ms || 0); recorder.muteFor(ms || 0, true); setTimeout(then, (ms || 0) + 150); }, () => then()); }
  function interruptPrompt(by) { player.stopSay(); sayingUntil = 0; announcingUntil = 0; recorder.unmute(); log('vc', `prompt interrupted by ${by}`); }

  // ---- on-device keywords ----
  /** "attention" opens a session at once (pausing playback, no upload); "potato" from standby enters command mode; "tornado" cancels; "tomato" ends command mode or sends. */
  function onKwsHit(word) {
    if (!voiceControl || !kwsEnabled() || ctx.dictating() || Date.now() - (kwsHitMs[word] ?? 0) < 1500) return;   // the same word twice in a row is one hit; a different word may follow at once ("attention potato")
    kwsHitMs[word] = lastHitAt = Date.now();
    if (Date.now() < sayingUntil && word !== 'TOMATO') interruptPrompt(word);   // "attention" right after a switch cuts "Now on X" short
    log('vc', `on-device keyword: ${word}`);
    if (word === 'ATTENTION' && startWord() === 'attention' && !vc && !choiceOpen()) {
      if (!lat.has('sw')) lat.begin('sw', 'startword');
      lat.stage('sw', 'kws-local');
      openSession('');
    } else if (word === 'TORNADO' && cancelWord() === 'tornado') {
      if (vc) cancelSession(); else if (pendingChoice) cancelChoice();
    } else if (word === 'POTATO' && commandWord() === 'potato' && !vc && !choiceOpen()) {   // the command word alone enters command mode: no start word first, one sound
      openSession('', true);
      if (vc) vc.unconfirmed = true;   // the spotter can mistake the start word for this one: the transcript of the clip that opened it decides (handleCommandClip)
      enterCommandMode('');
    } else if (word === 'TOMATO' && vc?.mode === 'command') {
      leaveCommandMode('end word on device');
    } else if (word === 'TOMATO' && vc && normalize(resumePhrase()) === 'tomato') {
      // 🧑 "I was talking, and then your response came": the spotter can hit mid-sentence. A real end word is followed by quiet; if speech goes on, the server transcript decides.
      const session = vc;
      setTimeout(() => {
        if (vc !== session || session.ending) return;
        if (recorder.quietMs() < END_WORD_QUIET_MS) { log('vc', 'end word on device ignored: speech continues, the transcript decides'); return; }
        sendVoiceReplyNow('end word on device');
      }, END_WORD_CHECK_MS);
    }
  }
  function onCommandHit(tag) {
    const inCommand = vc?.mode === 'command';
    if ((!inCommand && !choiceOpen()) || (tag === lastLocalTag && Date.now() - lastLocalTagMs < 1000)) return;
    lastLocalTag = tag; lastLocalTagMs = lastHitAt = Date.now();
    if (Date.now() < sayingUntil) interruptPrompt(tag);
    log('vc', `on-device command: ${localPhrases.get(tag) ?? tag.toLowerCase().replace(/_/g, ' ')} (${Math.round(Date.now() - (lastCommandWindowMs || Date.now()))}ms into the window)`);
    if (choiceOpen() && (tag === 'POSITIVE' || tag === 'NEGATIVE')) {   // the switch question: answered at once
      lastLocalCommandMs = Date.now(); lat.stage('switch', 'answer-heard');
      const {names, confirm} = pendingChoice; pendingChoice = null;
      closeSessionForChoice();
      if (tag === 'POSITIVE' && confirm) { ctx.switchTopic(names[0], true); vcIdle(); }
      else { recorder.beep('cancel'); vcIdle(); ctx.maybeAutoplay(); }
      return;
    }
    const num = {NUMBER_ONE: 0, NUMBER_TWO: 1, NUMBER_THREE: 2}[tag];
    if (num !== undefined) {   // 🧑 "say number one as soon as that is the correct agent": answered on-device, even mid-list
      if (!choiceOpen() || pendingChoice.confirm || !pendingChoice.names[num]) return;
      lastLocalCommandMs = Date.now(); lat.stage('switch', 'answer-heard');
      const name = pendingChoice.names[num]; pendingChoice = null;
      closeSessionForChoice(); ctx.switchTopic(name, true); vcIdle(); return;
    }
    if (!inCommand) return;
    if (tag === 'SKIP_FORWARD' || tag === 'SKIP_BACKWARD') {   // runs once now; a spoken count ("skip forward three") is added when the transcript arrives
      // 🧑 "skip backward, and with a number. It tries to skip too quickly, and sometimes doesn't register that number": the skip now waits for the transcript's count; one step if none comes.
      const dir = tag === 'SKIP_FORWARD' ? 1 : -1;
      clearTimeout(lastLocalNav?.timer);
      // Root cause of "two beeps, jumps back one block": a fixed 2.5 s wait raced the transcript (1.9-2.6 s), so a late count lost; and a tick sounded here plus another in runCommands. Now: silent here (playback paused), wait while the clip is still being spoken or transcribed (cap 6 s), one "Back N" acknowledgement from runCommands.
      const t0 = Date.now(), fire = () => {
        if (lastLocalNav?.dir !== dir) return;
        if ((recorder.inUtterance() || clipsInFlight || Date.now() - t0 < SKIP_IDLE_MS) && Date.now() - t0 < SKIP_COUNT_WAIT_MS) { lastLocalNav.timer = setTimeout(fire, 150); return; }
        lastLocalNav = {dir, ms: Date.now(), skipped: true}; log('vc', 'no count in time: skipping one'); runCommands(dir > 0 ? 'skip forward' : 'skip backward');
      };
      lastLocalNav = {dir, ms: t0, timer: setTimeout(fire, 150)};
      if (['playing', 'loading'].includes(player.position().status)) player.pause();
      log('vc', 'skip heard on device: acknowledged, waiting for the count (silent)'); return;   // does not set lastLocalCommandMs: the clip still goes to the server for the count
    }
    lastLocalCommandMs = Date.now();
    runCommands(localPhrases.get(tag) ?? tag.toLowerCase().replace(/_/g, ' '));
  }
  function setCommands() { const phrases = commandPhrases(model.topics().slice(0, 40).map(t => ctx.topicOf(t.name))); localPhrases = new Map(phrases.map(p => [p.tag, p.say ?? p.text])); kwsSpotter.setCommands(phrases); }
  /** Rebuilds the command spotter ahead of time (about 0.7 s of work) whenever the agent list changes, so opening command mode never waits for it. */
  function prebuildCommands() {
    clearTimeout(prebuildTimer);
    prebuildTimer = setTimeout(() => { if (kwsSpotter.ready() && kwsEnabled() && !vc) setCommands(); }, 2500);
  }
  function armLocalCommands() {
    lastCommandWindowMs = Date.now();
    if (!kwsSpotter.ready() || !kwsEnabled()) return;
    setCommands(); kwsSpotter.resetCommands();
  }

  // ---- server path: every clip ----
  /** Speech while idle or playing only records; the transcript decides (it must start with the start word). Playback volume is left alone. */
  function resumeBarge() {
    if (!bargePaused) return;
    bargePaused = false;
    if (player.position().status === 'paused') { log('vc', 'not a command: resuming in place'); player.resume(); }
    vcIdle();
  }
  function onBarge() {
    if (ctx.dictating()) return;
    if (!vc) lat.begin('sw', 'startword');   // speech onset
    log('vc', `speech detected while ${player.position().status}${vc ? ' (session open)' : ''}`);
    utterancePending = true; clearTimeout(utteranceTimer); utteranceTimer = setTimeout(utteranceDone, 6000);
    if (vc) { clearTimeout(vc.timer); clearTimeout(vc.emptyTimer); vc.timer = vc.emptyTimer = 0; }
    setVcState('heard', 'hearing');
  }
  let pieceBusy = false;   // a refresh of the growing piece is being transcribed: the next one waits for it
  let clipsInFlight = 0;   // clips whose transcription is still running
  async function onCommandClip(blob, meta = {}) {
    if (ctx.dictating()) return;
    if (!vc && !pendingChoice && kwsSpotter.ready() && kwsEnabled() && Date.now() - lastHitAt > (meta.ms ?? 0) + 1500) { await new Promise(r => setTimeout(r, LATE_HIT_WAIT_MS)); if (ctx.dictating()) return; }   // 🧑 2026-11-26 a start word's hit arrived 0.1 s after its clip was dropped as keyword-less
    if (vc && meta.piece) { const e = vc.pieces.get(meta.piece.id) ?? (vc.pieces.set(meta.piece.id, {text: '', part: {blob, text: null, fromCommand: false, piece: meta.piece.id, applied: 0}}), vc.pieces.get(meta.piece.id)); if (!(vc.parts ??= []).includes(e.part)) vc.parts.push(e.part); Object.assign(e.part, {blob, seq: meta.piece.seq}); if (meta.final) e.part.text = null; meta.part = e.part; }   // one part per piece: each refresh replaces its audio and makes its text stale until the new transcript arrives
    else if (vc) { meta.part = {blob, text: vc.mode === 'command' ? '' : null, fromCommand: vc.mode === 'command'}; (vc.parts ??= []).push(meta.part); if (vc.unconfirmed && Date.now() - (meta.ms ?? 0) <= vc.openedAt + 300) { meta.part.opening = true; vc.unconfirmed = false; } }   // kept so a spotted end word can send the audio itself; speech in command mode is never reply text
    clipsInFlight++; if (meta.piece && !meta.final) pieceBusy = true;
    try { await handleCommandClip(blob, meta); } finally { clipsInFlight--; if (meta.piece) pieceBusy = false; if (vc && !vc.ending && !vc.timer && !vc.emptyTimer && !recorder.inUtterance()) armSessionTimer(); utterancePending = false; clearTimeout(utteranceTimer); if (!userSpeaking()) ctx.maybeAutoplay(); }
  }
  /** Every start-word clip costs a Zulip upload plus messages: cap them, and back off in a noisy room instead of exhausting the rate limit. */
  function noteStartResult(useful) {
    emptyRun = useful ? 0 : emptyRun + 1;
    if (emptyRun >= EMPTY_RUN_BACKOFF && Date.now() > backoffUntil) { backoffUntil = Date.now() + BACKOFF_MS; recorder.setBackoff(true); log('vc', `noisy room: ${emptyRun} useless clips in a row, stricter detection for 60 s`); setTimeout(() => { recorder.setBackoff(false); emptyRun = 0; }, BACKOFF_MS); }
  }
  /** Not a start word: back to standby. */
  const standby = () => { recorder.setReplyMode(false); resumeBarge(); vcIdle(); };
  async function handleCommandClip(blob, meta) {
    if (meta.part?.sentAway) return;   // the reply was already sent: this clip needs no transcript
    if (!meta.piece && Date.now() - lastLocalCommandMs < (meta.ms ?? 0) + 800 && lastLocalCommandMs) { log('vc', 'clip already handled on-device: not sent to the server'); if (vc?.parts?.length) vc.parts.at(-1).text ||= ''; if (vc?.mode === 'command') armCommandTimer(); return; }
    log('vc', `clip ${Math.round(meta.ms ?? 0)}ms (${Math.round(meta.speechMs ?? 0)}ms speech) ${vc ? 'in reply' : pendingChoice ? 'for the question' : 'for the start word'}`);
    lat.stage('sw', 'clip-end');
    const graceReopen = !vc && !pendingChoice && !!emptyClosedAt && Date.now() - (meta.ms ?? 0) < emptyClosedAt + EMPTY_GRACE_MS;   // 🧑 2026-11-26 speech 0.7 s after an empty session timed out was dropped: it is uploaded, and counts as the reply
    if (graceReopen) log('vc', 'speech right after an empty session timed out: uploading it as the reply');
    if (!vc && !pendingChoice && kwsSpotter.ready() && kwsEnabled() && Date.now() - lastHitAt > (meta.ms ?? 0) + 1500 && !graceReopen) { log('vc', 'start-word clip with no local keyword: not uploaded'); standby(); return; }   // the on-device spotter hears start words; an upload per noise clip is the biggest source of Zulip calls
    if (!vc && !pendingChoice) {
      startWordUploads = startWordUploads.filter(t => Date.now() - t < 60000);
      if (startWordUploads.length >= UPLOADS_PER_MIN || zulip.rateLimitedForMs() > 0) { log('vc', `dropped a clip without uploading (${startWordUploads.length} uploads this minute${zulip.rateLimitedForMs() ? ', Zulip rate limit' : ''})`); recorder.setReplyMode(false); vcIdle(); return; }
      startWordUploads.push(Date.now());
    }
    const listening = !!vc || choiceOpen();
    // Domain words bias reply dictation only; never a prompt that names the control words: the model echoes it back on noise. Start-word clips also get keyword spotting on the server.
    let r = await voice.stt(blob, vc && vc.mode !== 'command' && (meta.speechMs ?? 0) >= 1500 ? (getLang() === 'zh' ? DOMAIN_PROMPT_ZH : DOMAIN_PROMPT) : undefined, !listening, {patient: !!meta.final || (!meta.piece && !!vc)});
    // 🧑 "if the front end. Detects something like the start words. But confidence is low. Then. We should request the backend to do it again. And confirm."
    const lowConfidence = r.ok && !listening && (r.noSpeechProb > START_MAX_NO_SPEECH || r.avgLogprob < START_MIN_LOGPROB);
    if (lowConfidence && Date.now() - lastHitAt < 3000) {
      log('vc', 'local start-word hit with low server confidence; retrying backend transcription');
      const retry = await voice.stt(blob, undefined, true);
      if (retry.ok) r = retry;
    }
    if (!r.ok && meta.piece && !meta.final) { log('vc', `piece refresh failed: ${r.error}`); return; }   // a refresh is only a preview; the final piece goes out as audio if it fails too
    if (!r.ok) {   // a transient failure is silent: inside a reply the clip stays as audio and goes out with the reply; the user is told what to do only once
      log('vc', `stt failed: ${r.error}`);
      if (vc?.mode === 'command') {   // 🧑 command mode with no connection gave zero feedback: say what still works (the on-device words), at most every 6 s
        if (Date.now() - (vc.toldCommandOffline ?? 0) > 6000) { vc.toldCommandOffline = Date.now(); recorder.beep('empty'); toast(tr('vc.noConnToast')); sayAndMute(tr('say.noConnCmd')); }
      } else if (vc && !vc.toldOffline) { vc.toldOffline = true; sayAndMute(tr('say.connLost')); }
      if (vc) armSessionTimer();
      resumeBarge(); vcIdle(); return;
    }
    if (meta.piece && !meta.final && meta.piece.seq <= 1 && (meta.speechMs ?? 0) < 1000 && r.text.trim().length <= 4) { log('vc', `dropped a short first preview "${r.text.trim()}" (${Math.round(meta.speechMs ?? 0)}ms speech)`); return; }   // noise before the words ("二千神。"): a later refresh carries the real text
    if (meta.piece && (!vc || !vc.parts?.includes(meta.part))) return;   // the session it belonged to has ended (sent or cancelled)
    let heard = r.text.trim();
    { const w = heard.split(/\s+/).filter(Boolean); if (w.length >= 2 && removeControlWords(removeControlWords(w, 'tomato'), 'attention').length === 0 && w.some(x => soundsLike('tomato', [x])) && w.some(x => soundsLike('attention', [x]))) { log('vc', `dropped "${heard}": only control words, an echo of noise`); heard = ''; } }
    if (meta.part) { if (meta.part.sentAway) return; if (meta.piece) { if (meta.piece.seq < meta.part.applied) return; meta.part.applied = meta.piece.seq; if (meta.piece.seq === meta.part.seq) meta.part.text = heard; } else meta.part.text = heard; }   // an older refresh never overwrites a newer one   // already sent from the audio: nothing more to do with this transcript
    lat.stage('sw', 'stt-roundtrip', `relay q${r.timings?.queueMs ?? '?'} w${r.timings?.workMs ?? '?'} lag${r.timings?.eventLagMs ?? '?'}`);
    log('vc', `heard "${heard}" (speech ${Math.round(meta.speechMs ?? 0)}ms, no_speech ${r.noSpeechProb?.toFixed(2)}, logprob ${r.avgLogprob?.toFixed(2)})`);
    if (!listening && normalize(heard) && ((meta.speechMs ?? 1e9) < START_MIN_SPEECH_MS || r.noSpeechProb > START_MAX_NO_SPEECH || r.avgLogprob < START_MIN_LOGPROB)) {
      log('vc', 'rejected as noise: too short or low confidence'); standby(); return;
    }
    if (!normalize(heard) && meta.piece && vc) { setPiece(meta.piece.id, ''); if (vc.mode !== 'command') armSessionTimer(); vcIdle(); return; }
    if (!normalize(heard)) { if (!vc) { noteStartResult(false); recorder.setReplyMode(false); resumeBarge(); } else armSessionTimer(); vcIdle(); return; }   // noise must not cancel the end-of-reply timer for good
    if (lastLocalNav) {   // "skip forward three": the device heard the skip and waits for this transcript's count
      const said = normalize(afterStartWord(heard) ?? heard).replace(new RegExp(`^${commandWord()}\\s+`), ''), nav = navCommand(said), d = lastLocalNav.dir;
      const skipped = lastLocalNav.skipped && Date.now() - lastLocalNav.ms < 10000;
      clearTimeout(lastLocalNav.timer); lastLocalNav = null;
      if (skipped) {   // the one step already happened: a late count adds the rest
        if (nav && nav.dir === d && nav.count > 1) log('vc', `late count from the transcript ignored: ${nav.count}`);   // a second move after playback started was the flicker
        if (nav && nav.dir === d) { vcIdle(); return; }
      } else if (nav && nav.dir === d) { log('vc', `skip count from the transcript: ${nav.count}`); runCommands(said); vcIdle(); return; }
      else runCommands(d > 0 ? 'skip forward' : 'skip backward');
    }
    if (choiceOpen()) {   // while a switch question is open, answers are matched first, even inside a session
      const w = normalize(afterStartWord(heard) ?? heard);
      const pick = choiceAnswer(w, pendingChoice, rank);
      if (pick) { pendingChoice = null; lat.stage('switch', 'answer-heard'); closeSessionForChoice(); ctx.switchTopic(pick, true); vcIdle(); return; }   // the switch tone and "Now on X" follow; no voice state lingers
      if (NEGATIVE_RE.test(w) || CANCEL_RE.test(w)) { pendingChoice = null; closeSessionForChoice(); recorder.beep('cancel'); vcIdle(); ctx.maybeAutoplay(); return; }
      if (!vc) { log('vc', `ignored during the switch question: "${heard}"`); vcIdle(); return; }   // the start word must not open a reply while we wait for the answer
    }
    if (!vc) { onStandbyTranscript(heard, r, graceReopen); return; }
    const body = normalize(afterStartWord(heard) ?? heard);   // a clip that begins with the start word (the on-device spotter already opened the session) still holds what follows it
    if (meta.part?.opening && vc.mode === 'command') {   // 🧑 2026-11-26 the spotter heard POTATO for a start word and every sentence after it was dropped
      const first = normalize(heard).split(' ')[0] ?? '';
      if (afterStartWord(heard) !== null && !isCommandWord(first)) {
        log('vc', `local command word was wrong: the clip says "${heard}"; input mode`);
        clearTimeout(vc.timer); vc.mode = 'input'; vc.sawCommand = false; meta.part.fromCommand = false; vc.parts.forEach(p => { p.fromCommand = false; });
        recorder.beep('ready'); setVcState('dictating', 'sayReply');
        addToSession(heard); return;
      }
    }
    if (vc.mode === 'command') {   // COMMAND mode: the closed list only; the end word returns to input, cancel discards
      if (stripResumePhrase(body) !== null && !body.split(' ').filter(Boolean).some(w => CANCEL_RE.test(w))) { leaveCommandMode('end word'); return; }
      runCommands(body, heard); return;
    }
    if (CANCEL_RE.test(body)) { cancelSession(); return; }   // discards this reply and carries on
    addToSession(heard, meta.piece);
  }
  /** A transcript with no session open: it opens one only after the start word (or the command word, or right after a send). */
  function onStandbyTranscript(heard, r, graceReopen = false) {
    // With server keyword spotting on (r.kws is a list): it must have heard ATTENTION, and Whisper must confirm the standalone word.
    if (graceReopen && afterStartWord(heard) === null && !isCommandWord(normalize(heard).split(' ')[0] ?? '')) { noteStartResult(true); openSession(heard.trim()); return; }
    const kwsOn = Array.isArray(r.kws) && startWord() === 'attention' && Date.now() - lastReplyEndMs >= FOLLOW_UP_GRACE_MS;   // the server spots "attention" only; a follow-up right after a send needs no start word
    const commandFirst = isCommandWord(normalize(heard).split(' ')[0] ?? '');   // "potato next two": the command word alone starts command mode
    if (kwsOn && !commandFirst && !r.kws.includes('ATTENTION')) { noteStartResult(false); log('vc', `ignored: "${heard}" (keyword spotter did not hear the start word)`); standby(); return; }
    let rest = afterStartWord(heard);
    // Whisper sometimes drops the leading start word but keeps the command after it: with a spotter hit, a bare command still counts.
    const commandLike = t => { const n = normalize(t); return CANCEL_RE.test(n) || n === 'send'; };
    if (commandFirst && rest === null) rest = heard;
    if (kwsOn && rest === null && commandLike(heard)) rest = heard;
    if (kwsOn && rest !== null && afterStartWord(heard) === null && !commandLike(heard) && !commandFirst) { log('vc', `ignored: "${heard}" (transcript is not the start word)`); standby(); return; }
    const followUp = Date.now() - lastReplyEndMs < FOLLOW_UP_GRACE_MS;
    if (rest === null && followUp && commandLike(heard)) rest = heard;   // a command right after a send is still a command, never reply text
    // 🧑 "The system picked up garbage": room talk 2 s after a send opened a reply with no start word and was sent after the silence. Speech after a send now needs the start word like any other.
    if (rest === null && followUp) log('vc', `right after a send, but no start word: "${heard}"`);
    if (rest === null) { noteStartResult(false); resumeBarge(); log('vc', `ignored: "${heard}" (no start word)`); toast(tr('vc.ignored', {heard: heard.slice(0, 60)})); recorder.setReplyMode(false); vcIdle(); return; }
    noteStartResult(true);
    if (commandFirst && afterStartWord(heard) === null) { openSession('', true); enterCommandMode(heard.trim().split(/\s+/).slice(1).join(' ')); return; }   // "potato next two" from standby: command mode directly
    openSession(rest);
  }

  // ---- command mode ----
  function enterCommandMode(rest) {
    if (!vc) return;
    clearTimeout(vc.timer); clearTimeout(vc.emptyTimer);
    const first = vc.mode !== 'command';
    recorder.setPieceMode(false); recorder.flush();
    vc.mode = 'command'; vc.sawCommand = true;
    if (first) { armLocalCommands(); recorder.beep('command'); log('vc', 'command mode'); log('lat', `command mode ready ${Math.round(Date.now() - (vc.openedAt ?? Date.now()))}ms after the session opened`); }
    setVcState('command', 'command');
    toast(tr('vc.commandToast'));
    armCommandTimer();
    if (rest) runCommands(rest);
  }
  function armCommandTimer() { clearTimeout(vc.timer); vc.timer = setTimeout(() => leaveCommandMode('silence'), COMMAND_SILENCE_MS + Math.max(0, sayingUntil - Date.now())); }   // our own spoken prompt does not eat the window
  /** Back to INPUT mode with the draft kept. */
  function leaveCommandMode(why) {
    if (!vc || vc.mode !== 'command') return;
    if (why === 'silence' && !vc.text.trim()) { clearTimeout(vc.timer); clearTimeout(vc.emptyTimer); log('vc', 'command mode left (silence) with nothing dictated: standby'); setDraft(vc.base); vc = null; recorder.setReplyMode(false); recorder.beep('done'); ctx.resumeAfterPause(); vcIdle(); return; }   // 🧑 an unrecognized command must not end in input mode
    clearTimeout(vc.timer); vc.mode = 'input'; recorder.setPieceMode(true); log('vc', `command mode left (${why}); back to input`);
    recorder.beep('ready'); setVcState('dictating', 'sayReply');
    armSessionTimer();
  }
  /** Words after the command word, executed in order: "next three back one switch to human manager". Unknown words are ignored; nothing here becomes text. */
  function runCommands(text, raw = '') {
    const tokens = normalize(text).split(' ').filter(Boolean).filter(t => !isCommandWord(t));
    const done = [];
    const said = [];   // what was done, in words: a bare sound never says which command it was
    const tick = (name, words = name) => { done.push(name); said.push(words); if (name !== 'navigate') recorder.beep('tick'); };   // a navigation is acknowledged by its spoken "Back N" alone: no extra beep
    let navigated = false, left = false; const ignored = [], navs = [];
    const finishSession = (discard = false) => { if (!vc) return; clearTimeout(vc.timer); clearTimeout(vc.emptyTimer); setDraft(discard ? vc.base : vc.base + (vc.text ? (vc.base ? ' ' : '') + vc.text : '')); ctx.saveDraft(); vc = null; recorder.setReplyMode(false); };
    while (tokens.length && vc) {
      const t = tokens[0], two = tokens.slice(0, 2).join(' ');
      if (SEND_LOG_RE.test(two) || SEND_LOG_RE.test(tokens.slice(0, 3).join(' '))) { tokens.splice(0, SEND_LOG_RE.test(two) ? 2 : 3); log('vc', 'send log by voice'); tick('log', tr('say.sendingLog')); ctx.sendLog('voice command').then(r => { if (!r?.ok) sayAndMute(tr('say.logFailed')); }); continue; }
      if (NEXT_AGENT_RE.test(tokens.slice(0, 4).join(' ')) || NEXT_AGENT_RE.test(tokens.slice(0, 3).join(' ')) || NEXT_AGENT_RE.test(two)) { log('vc', 'next agent with unread messages'); done.push('next agent'); recorder.beep('tick'); finishSession(true); bargePaused = false; ctx.switchNextUnread(); left = true; break; }
      if (SWITCH_BACK_RE.test(tokens.slice(0, 3).join(' ')) || SWITCH_BACK_RE.test(two)) { log('vc', 'switch back to the previous agent'); done.push('switch back'); recorder.beep('tick'); finishSession(true); bargePaused = false; ctx.switchBack(); left = true; break; }
      if (/^(switch|switched|switching|go)$/.test(t) && /^(to|agent|agents)$/.test(tokens[1] ?? '')) {   // Whisper often hears "switched to"; the name is free-form: it takes the rest of the utterance and ends the chain
        const name = tokens.slice(/^(agent|agents)$/.test(tokens[1]) && tokens[2] === 'to' ? 3 : 2).join(' ');
        done.push('switch'); recorder.beep('tick'); finishSession(true); voiceSwitch(name); left = true; break;
      }
      if (CANCEL_RE.test(two) || CANCEL_RE.test(t)) { cancelSession(); return done; }
      const take = [5, 4, 3, 2, 1].find(n => navCommand(tokens.slice(0, n).join(' ')));   // longest prefix that is one navigation command ("next three", "previous message", "back")
      if (take) { const nav = navCommand(tokens.splice(0, take).join(' ')); navs.push(nav); tick('navigate', `${nav.unread ? tr('nav.unread') : nav.restart ? tr('nav.restart') : nav.dir === 0 ? tr('nav.again') : nav.whole ? (nav.dir > 0 ? tr('nav.nextMsg') : tr('nav.backMsg')) : nav.dir > 0 ? tr('nav.next') : tr('nav.back')}${nav.count > 1 ? ' ' + nav.count : ''}`); navigated = true; continue; }
      if (/^(stop|pause|wait)$/.test(t)) { tokens.shift(); player.pause(); ctx.holdPlayback?.(true); tick('pause', tr('say.paused')); continue; }
      if (/^(play|resume|continue)$/.test(t)) { tokens.shift(); tick('play', tr('say.playing')); navigated = true; ctx.holdPlayback?.(false); ctx.resumeAfterPause(); continue; }
      log('vc', `command mode ignored "${t}"`); tokens.shift(); ignored.push(t);
    }
    const kept = raw && vc ? withoutStrayEndWords((afterStartWord(raw) ?? raw).trim()) : '';
    if (!done.length && ignored.length && vc && kept) {   // 🧑 2026-11-26 "a sentence heard in command mode that isn't a command must not be thrown away": it joins the reply text; "tomato" or silence returns to input mode with it
      log('vc', `not a command: kept as reply text "${kept}"`); appendText(kept); showSessionText();
      recorder.beep('empty'); toast(tr('vc.notCommandToast', {kept: kept.slice(0, 80), resume: resumePhrase()})); sayAndMute(tr('say.notCommand'));
    } else if (!done.length && ignored.length && vc) {   // heard something, understood nothing: say so and show the commands (silence would look like a hang)
      log('vc', `command not recognized: "${ignored.join(' ')}"`); recorder.beep('empty'); toast(tr('vc.unrecognizedToast', {heard: ignored.join(' ')})); sayAndMute(tr('say.unrecognized'));
    }
    if (done.length) log('vc', `commands: ${done.join(', ')}`);
    // 🧑 "It should, say. Back, with the number first, and then, start playing": navigation waits until the confirmation has been spoken, so the two never overlap.
    if (said.length && navs.length) sayThen(said.join(tr('sep')), () => { for (const nav of navs) ctx.navigate(nav); });
    else if (said.length) sayAndMute(said.join(tr('sep')));   // e.g. "paused"
    else for (const nav of navs) ctx.navigate(nav);
    if (navigated && vc && !left) { finishSession(); vcIdle(); left = true; }
    if (vc && !left) armCommandTimer();
    return done;
  }

  // ---- switching by voice ----
  function cancelChoice() { if (pendingChoice) { pendingChoice = null; recorder.beep('cancel'); toast(tr('vc.switchCancelled')); ctx.maybeAutoplay(); } }
  function askChoice(names, confirm, prompt) {
    pendingChoice = {names, until: Date.now() + CHOICE_MS, confirm};
    setTimeout(() => { if (pendingChoice && Date.now() >= pendingChoice.until) cancelChoice(); }, CHOICE_MS + 500);
    toast(prompt); sayAndMute(prompt); armLocalCommands();
  }
  /** Answering the switch question closes any session without sending it. */
  function closeSessionForChoice() { if (vc) { clearTimeout(vc.timer); clearTimeout(vc.emptyTimer); setDraft(vc.base); vc = null; } recorder.setReplyMode(false); }
  /** "switch to <name>": one clear match asks "Switch to X?"; otherwise read the top 3 and accept a number or name. */
  function voiceSwitch(spoken) {
    lat.begin('switch', 'switch');
    if (!normalize(spoken)) {   // "switch to" with no name: wait for the name
      const names = model.topics().slice(0, 3).map(t => t.name);
      pendingChoice = {names, until: Date.now() + CHOICE_MS, confirm: false}; setTimeout(() => { if (pendingChoice && Date.now() >= pendingChoice.until) cancelChoice(); }, CHOICE_MS + 500);
      toast(tr('vc.whichToast')); sayAndMute(tr('say.which')); armLocalCommands(); return;
    }
    const ranked = rank(spoken);
    log('vc', `switch to "${spoken}" -> ${ranked.slice(0, 3).map(x => `${x.t.name}:${x.score.toFixed(2)}`).join(', ') || 'no match'}`);
    if (!ranked.length) { log('vc', 'no agent matches'); toast(tr('vc.noAgentToast', {name: spoken})); sayAndMute(tr('say.noAgent', {name: spoken})); return; }
    const clear = clearWinner(ranked), names = clear ? [ranked[0].t.name] : ranked.slice(0, 3).map(x => x.t.name);
    const readable = n => n.replace(/[_-]/g, ' ');
    // 🧑 "if I say "switch to some agent name," and ... only found one match, we don't need to wait for me to say "positive." We just switch there immediately" / "Maybe this would be a configuration that we can toggle."
    if (clear && !store.get('voice2-switch-confirm', false)) { log('vc', `one clear match: switching to ${names[0]} at once`); closeSessionForChoice(); ctx.switchTopic(names[0], true); vcIdle(); return; }
    askChoice(names, clear, clear ? tr('say.switchTo', {name: readable(names[0])}) : names.map((n, i) => tr('say.choice', {n: i + 1, name: readable(n)})).join(' '));
    lat.stage('switch', 'question-shown');
  }
  /** Earcon plus "Now on <agent>", so a switch to an agent with nothing to play is still noticeable; unread messages play afterwards. Silent unless voice control is on. */
  function announceSwitch(topic, confirmed = false) {
    if (!voiceControl) return;
    const willPlay = model.unreadAgentMessages(topic).some(m => itemsOf(m).length);
    const resumeLater = ms => setTimeout(() => { if (ctx.current() === topic) ctx.maybeAutoplay(); }, ms);
    if (confirmed) { recorder.beep('command'); log('vc', 'switch confirmed: click'); }   // heard at once: the same double click as entering command mode
    if (confirmed && willPlay) {   // the messages start right after the click: "Now on X" would only delay or overlap them
      announcingUntil = Date.now() + COMMAND_SOUND_MS + 300; resumeLater(COMMAND_SOUND_MS + 350);
      return;
    }
    announcingUntil = Date.now() + 4000;
    recorder.muteFor(3500, true); sayingUntil = Date.now() + 3500;
    const speak = () => player.say(tr('say.nowOn', {name: topic})).then(ms => { sayingUntil = Date.now() + ms; recorder.muteFor(ms, true); announcingUntil = Date.now() + ms + 300; resumeLater(ms + 350); });
    if (confirmed) setTimeout(() => { if (ctx.current() === topic) speak(); else announcingUntil = 0; }, COMMAND_SOUND_MS + 100); else speak();
  }

  // ---- input mode: the reply session ----
  let lastVoiceMs = 0, lastPulseMs = 0;
  setInterval(() => showModeBanner(), 500);   // hides it after MODE_BANNER_MS or when the session ends
  setInterval(() => {   // the mode beat: only in an open session, only while nobody (neither the human nor the app) is making sound
    if (!vc || vc.ending || recorder.inUtterance() || Date.now() < sayingUntil || recorder.earconLeftMs() > 0) { if (recorder.inUtterance()) lastVoiceMs = Date.now(); return; }
    if (['playing', 'loading'].includes(player.position().status) || lastLocalNav) return;
    const quietSince = Math.max(lastVoiceMs, lastPulseMs, vc.openedAt ?? 0);
    if (Date.now() - quietSince < MODE_PULSE_MS) return;
    lastPulseMs = Date.now(); recorder.beep(vc.mode === 'command' ? 'pulseCommand' : 'pulseInput'); showModeBanner(true);
  }, 500);
  function openSession(rest, quiet = false) {
    bargePaused = false;
    if (['playing', 'loading'].includes(player.position().status)) player.pause();
    lat.finish('sw', 'pause-applied');
    vc = {base: draft.value, text: '', fixed: '', pieces: new Map(), timer: 0, emptyTimer: 0, openedAt: Date.now()};
    recorder.setReplyMode(true); recorder.setPieceMode(!quiet);   // input mode: one growing piece instead of a clip per pause
    if (!quiet) recorder.beep('ready');   // quiet: the command sound follows
    log('vc', `session opened by start word; rest "${rest}"`);
    if (CANCEL_RE.test(normalize(rest))) { log('vc', 'cancel right after the start word'); vc = null; recorder.setReplyMode(false); recorder.beep('cancel'); ctx.resumeAfterPause(); vcIdle(); return; }
    if (rest) addToSession(rest);
    else { setVcState('dictating', 'sayReply'); vc.emptyTimer = setTimeout(closeEmptySession, EMPTY_SESSION_MS); }
  }
  /** Discards the open reply (restoring the draft), plays the cancel tone and carries on reading. */
  function cancelSession() {
    clearTimeout(vc.timer); clearTimeout(vc.emptyTimer);
    log('vc', 'cancel: reply discarded');
    setDraft(vc.base); ctx.saveDraft();
    vc = null; recorder.setReplyMode(false); recorder.beep('cancel');
    ctx.resumeAfterPause(); vcIdle();
  }
  const withoutStrayEndWords = t => removeControlWords(t.split(/\s+/).filter(Boolean)).join(' ');   // "Tomato." said mid-reply is a command, never text
  function addToSession(heard, piece = null) {
    if (hasCancel(heard, cancelWord())) { cancelSession(); return; }   // the rare cancel word fires wherever it is said
    const words = (afterStartWord(heard) ?? heard).split(/\s+/).filter(Boolean), at = words.findIndex(isCommandWord);
    if (at >= 0 && vc && vc.mode !== 'command') {   // in input mode the command word is not a command
      log('vc', 'command word inside dictation: ignored, not a mode switch'); words.splice(at, 1); heard = words.join(' ');
      if (!heard.trim()) { if (piece) setPiece(piece.id, ''); armSessionTimer(); return; }
    } else if (at >= 0 && vc) {   // in command mode: what precedes the command word is reply text, what follows is commands
      const before = words.slice(0, at).join(' ');
      if (before) { appendText(withoutStrayEndWords(before)); showSessionText(); }
      enterCommandMode(words.slice(at + 1).join(' ')); return;
    }
    if (endsCancel(heard, cancelWord())) { cancelSession(); return; }   // "... cancel" at the end of any utterance cancels the whole reply
    clearTimeout(vc.emptyTimer); vc.emptyTimer = 0;
    if (stripResumePhrase(heard) !== null && !vc.sending) { const t = (afterStartWord(heard) ?? heard).trim(); if (piece) setPiece(piece.id, t); else { appendText(t); showSessionText(); } recorder.drop(); sendVoiceReplyNow('end word in the transcript'); return; }   // what follows the end word is dropped with the open piece   // no quiet window
    { const t = withoutStrayEndWords((afterStartWord(heard) ?? heard).trim()); if (piece) setPiece(piece.id, t); else { appendText(t); showSessionText(); } }
    setVcState('dictating', 'dictating');
    armSessionTimer();
  }
  /** (Re)starts the end-of-reply timer: end word + short silence, else the long silence; an empty session gets the empty timeout. */
  function armSessionTimer() {
    if (!vc || vc.ending) return;
    clearTimeout(vc.timer); clearTimeout(vc.emptyTimer);
    if (recorder.inUtterance()) return;   // still talking: the clip being recorded will arm the timer when its transcript arrives
    const silenceMs = stripResumePhrase(vc.text) !== null ? RESUME_SILENCE_MS : Number(store.get('voice2-long-silence-s', LONG_SILENCE_DEFAULT_S)) * 1000;
    if (vc.text.trim() && silenceMs > 0) vc.timer = setTimeout(finishVcReply, silenceMs);
    else if (!vc.text.trim()) vc.emptyTimer = setTimeout(closeEmptySession, EMPTY_SESSION_MS);
  }
  const untranscribed = () => !vc.sawCommand && (vc.parts ?? []).some(p => p.text === null && !p.sentAway);
  /** A session with nothing said never hangs: close it and carry on. */
  function closeEmptySession() {
    if (!vc || vc.text.trim()) return;
    // 🧑 2026-11-23 "I was in input mode. Said something. And then, when I said the end word, everything got dropped": `utterancePending` lapses 6 s after speech starts, so a
    // sentence over 8 s that began with the start word was cut off as an 'empty' session and its transcript then rejected. The microphone's own view of speech, and a clip still being transcribed, now also hold it open.
    if (utterancePending || recorder.inUtterance() || clipsInFlight) { vc.emptyTimer = setTimeout(closeEmptySession, 2000); return; }   // still talking (the clip's transcript is not back yet): never close mid-speech
    if (untranscribed()) { sendVoiceReplyNow('clips could not be transcribed here'); return; }   // offline: the audio is sent and transcribed by the server later
    log('vc', 'empty session timed out: resuming'); emptyClosedAt = Date.now();
    vc = null; recorder.setReplyMode(false); recorder.beep('empty');
    player.unlock();
    ctx.resumeAfterPause();
    vcIdle();
  }
  /** The end word was heard: no quiet window, no waiting for transcripts. The clip in progress is cut here and the service transcribes whatever is not yet text. */
  function sendVoiceReplyNow(why) {
    const reply = vc; if (!reply || reply.ending) return;
    reply.ending = true;
    recorder.flush();   // hands the last clip to onCommandClip, which records it in reply.parts
    clearTimeout(reply.timer); clearTimeout(reply.emptyTimer);
    // A piece whose newest audio has no transcript yet goes as audio (its older preview text is not trusted); finished text goes as text.
    let ready = joinText(reply.fixed, ...[...reply.pieces].sort((a, b) => a[0] - b[0]).filter(([, e]) => e.part.text !== null).map(([, e]) => e.text));
    if (why.includes('on device')) { const w = ready.trim().split(/\s+/); if (w.length && looksLikeClippedEndWord(w.at(-1))) { log('vc', `dropped the clipped end word "${w.at(-1)}"`); ready = w.slice(0, -1).join(' '); } }
    const items = [];
    if (reply.base.trim()) items.push({text: reply.base.trim()});
    if (ready.trim()) items.push({text: ready.trim()});   // everything already transcribed, in speaking order
    for (const p of reply.parts ?? []) { p.sentAway = true; if (p.text === null && !reply.sawCommand) items.push({blob: p.blob}); }   // clips still waiting for text go as audio (the relay joins them); after a command word the audio may hold commands: only text is trusted
    vc = null; recorder.setReplyMode(false); lastReplyEndMs = Date.now();
    log('vc', `${why}: sending now (${items.filter(i => i.blob).length} clips for the server to transcribe)`);
    if (!items.some(i => i.blob)) {   // the relay applies the same rules; nothing is even uploaded
      const asm = assembleReply(items.map(i => i.text ?? ''), {cancelWord: cancelWord(), commandWord: commandWord()});
      if (asm.discard || !asm.text) { log('vc', `reply not sent (${asm.why ?? 'only control words'})`); setDraft(reply.base); ctx.saveDraft(); recorder.beep('cancel'); ctx.resumeAfterPause(); vcIdle(); return; }
    }
    recorder.beep('done');
    ctx.sendLocal(ready || reply.text || '…', items);
    setDraft(''); ctx.saveDraft();
    setVcState('sending', 'sending');
    player.unlock(); ctx.resumeAfterPause(); vcIdle();
  }
  /** End word plus silence (or the long silence): drop the end word, send the reply if there is one, then resume reading. */
  function finishVcReply() {
    if (!vc || vc.ending) return;
    if (recorder.inUtterance()) { clearTimeout(vc.timer); vc.timer = setTimeout(finishVcReply, 1500); log('vc', 'end timer fired while you were still speaking: waiting'); return; }   // 🧑 "A buggy early stop, although I did not say the terminal word": the timer armed by an earlier transcript fired in the middle of a long clip   // a timer re-armed while a send was waiting for clips must not fire on a closed session
    if (pieceBusy) { clearTimeout(vc.timer); vc.timer = setTimeout(finishVcReply, 400); return; }   // the last refresh of the piece is still being transcribed
    if (untranscribed()) { sendVoiceReplyNow('clips still untranscribed'); return; }
    recorder.drop();   // the piece's text is complete; its audio is not needed
    const reply = vc, asm = assembleReply([stripResumePhrase(reply.text) ?? reply.text], {cancelWord: cancelWord(), commandWord: commandWord()}), stripped = asm.text;   // same rules as the relay: a cancel word discards it all, command speech and keywords are never text
    if (asm.discard || !stripped) log('vc', `reply not sent (${asm.why ?? 'only control words'}): "${reply.text}"`);
    vc = null; recorder.setReplyMode(false); lastReplyEndMs = Date.now();
    setDraft([reply.base.trim(), stripped].filter(Boolean).join(' ')); ctx.saveDraft();
    log('vc', `resume phrase + silence: reply "${stripped}"`);
    recorder.beep(stripped ? 'done' : 'empty');
    const key = normalize(stripped);
    const duplicate = key && lastVoiceReply.key === key && Date.now() - lastVoiceReply.ms < 120000;   // never send the same reply twice
    if (duplicate) { log('vc', 'duplicate reply suppressed'); recorder.beep('empty'); }
    if (stripped && !duplicate) { lastVoiceReply = {key, ms: Date.now()}; setVcState('sending', 'sending'); ctx.send(); }
    player.unlock();
    ctx.resumeAfterPause();
    vcIdle();
  }

  // ---- on/off ----
  async function loadKws(retry = true) {
    const r = await kwsSpotter.load();
    if (r.ok) prebuildCommands();
    if (!r.ok && retry && voiceControl && kwsEnabled()) { setTimeout(() => { if (voiceControl && kwsEnabled() && !kwsSpotter.ready()) loadKws(false); }, 8000); }
    if (!r.ok && voiceControl) { toast(tr('vc.kwsFailed')); setTimeout(() => $('set-kws').checked = kwsEnabled(), 0); }
  }
  async function setVoiceControl(on) {
    voiceControl = on; voiceListening = on; store.set('voice2-vc', on);
    $('set-voice').checked = on; if (on) vcIdle(); else setVcState('off', 'off');
    if (!on) { recorder.listen(false); kwsSpotter.unload(); return; }
    if (kwsEnabled()) loadKws(); else log('kws', 'on-device start word is OFF in Settings: start words take the server path (slower)');   // lazy: the model (about 21 MB over the wire, once) loads only when voice control is switched on
    const r = await recorder.listen(true);
    if (r.ok && !r.running) { voiceListening = false; setVcState('waiting', 'tap'); log('vc', 'armed, waiting for a tap'); return; }
    if (!r.ok) { toast(r.error); player.say(tr('say.micStopped')); voiceControl = false; store.set('voice2-vc', false); $('set-voice').checked = false; setVcState('off', 'off'); }
    log('vc', `voice control ${r.ok ? 'on' : 'failed'}`);
    if (r.ok && r.running) recorder.beep('ready');
  }

  // ---- settings sheet ----
  
  const textSetting = (id, key, fallback, clean) => { $(id).value = store.get(key, fallback); $(id).onchange = e => store.set(key, clean(e.target.value.trim()) || fallback); };
  textSetting('set-phrase', 'voice2-resume-phrase', RESUME_DEFAULT, v => v);
  textSetting('set-start', 'voice2-start-word', START_DEFAULT, v => v.toLowerCase());
  textSetting('set-cancel', 'voice2-cancel-word', CANCEL_WORD_DEFAULT, v => v.toLowerCase());
  textSetting('set-cmd', 'voice2-cmd-word', CMD_DEFAULT, v => v.toLowerCase());
  $('set-silence').value = store.get('voice2-long-silence-s', LONG_SILENCE_DEFAULT_S);
  $('set-silence').onchange = e => store.set('voice2-long-silence-s', Math.max(0, Number(e.target.value) || 0));
  $('set-kws').checked = kwsEnabled();
  { const box = $('set-switch-confirm'); if (box) { box.checked = store.get('voice2-switch-confirm', false); box.onchange = e => store.set('voice2-switch-confirm', e.target.checked); } }
  $('set-kws').onchange = e => { store.set('voice2-kws', e.target.checked); if (e.target.checked && voiceControl) loadKws(); else if (!e.target.checked) kwsSpotter.unload(); };
  $('set-voice').onchange = e => setVoiceControl(e.target.checked);
  $('listen').onclick = e => { e.stopPropagation(); setVoiceControl(!voiceControl); toast(voiceControl ? tr('vc.on') : tr('vc.offToast')); };

  return {
    recorder, userSpeaking, announceSwitch, prebuildCommands, cancelWord, commandWord,
    enabled: () => voiceControl,
    sessionOpen: () => !!vc,
    micQuietMs: () => recorder.quietMs(),
    /** A saved "on" re-arms after reload: try at once, else on the first tap anywhere. */
    relocalize() { setVcState(...vcShown); },
    start() { if (voiceControl) { $('set-voice').checked = true; setVcState('waiting', 'tap'); setVoiceControl(true); } },
    onGesture() { if (voiceControl && !recorder.active() && !voiceListening) setVoiceControl(true); },
    /** The draft was sent by hand: a later timer must not send it again. */
    soundLeftMs: () => recorder.earconLeftMs(),
    closeForManualSend() { if (vc) { clearTimeout(vc.timer); clearTimeout(vc.emptyTimer); vc = null; recorder.setReplyMode(false); log('vc', 'draft sent by hand: voice session closed'); } },
    debugLine: () => `on-device start word: setting ${kwsEnabled() ? 'on' : 'OFF'}, ${kwsSpotter.state().status}${kwsSpotter.state().detail ? ' (' + kwsSpotter.state().detail + ')' : ''}`,
  };
}
