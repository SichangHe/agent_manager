/**
 * Hands-free voice control: start word -> INPUT mode (reply dictation) -> command word -> COMMAND mode (a closed list, chainable, never text);
 * end word sends, cancel word discards. Owns the microphone (recorder), the on-device keyword spotter and the switch question.
 * Everything it does to playback, the feed or the composer goes through `ctx`.
 */
import {createRecorder, EARCON_NOTES, EARCON_NOTE_S} from './recorder.mjs';
import {createKeywordSpotter} from './kws.mjs';
import {assembleReply, looksLikeClippedEndWord, soundsLike, removeControlWords, DOMAIN_PROMPT} from './text.mjs';
import {itemsOf} from './model.mjs';
import {$, toast} from './chrome.mjs';
import {normalize, rankTopics, clearWinner, commandPhrases, afterStartWord as afterStart, stripResumePhrase as stripResume, isCommandWord as isCmd, isCancel, hasCancelWord as hasCancel, endsWithCancel as endsCancel,
  navCommand, choiceAnswer, NEGATIVE_RE, SWITCH_BACK_RE, NEXT_AGENT_RE, SEND_LOG_RE, RESUME_DEFAULT, START_DEFAULT, CMD_DEFAULT, CANCEL_WORD_DEFAULT, LONG_SILENCE_DEFAULT_S} from './grammar.mjs';

const RESUME_SILENCE_MS = 1000;   // plus the recorder's 0.7 s end-of-utterance silence: about 1.7 s of quiet after the phrase
const FOLLOW_UP_GRACE_MS = 6000;
const EMPTY_SESSION_MS = 8000;
const COMMAND_SILENCE_MS = 8000;
const EARCON_GAIN = {cancel: 0.85, command: 0.75, empty: 0.7};   // the quiet ones were the ones the human could not hear
const CHOICE_MS = 20000;
const START_MIN_SPEECH_MS = 250, START_MAX_NO_SPEECH = 0.6, START_MIN_LOGPROB = -1.0;
const UPLOADS_PER_MIN = 8, EMPTY_RUN_BACKOFF = 3, BACKOFF_MS = 60000;
/** Mode names as the human reads them (help text and toasts); rename here only. Log keys stay `command mode` etc. */
const MODE_NAMES = {standby: 'standby', input: 'input', command: 'command'};
const COMMAND_SOUND_MS = EARCON_NOTES.command.length * EARCON_NOTE_S * 1000;

/**
 * ctx: {store, log, lat, player, zulip, voice, model, current(), topicOf(key), draft (textarea), autosize(), saveDraft(), dictating(),
 *       maybeAutoplay(), resumeAfterPause(), switchTopic(topic, confirmed), switchBack(), navigate(nav), send(), sendLocal(text, items), onClip, onLevel}
 */
export function createVoiceControl(ctx) {
  const {store, log, lat, player, zulip, voice, model, draft} = ctx;
  const kwsSpotter = createKeywordSpotter({log});

  // ---- settings ----
  if (['over', 'kangaroo'].includes(store.get('voice2-resume-phrase', null))) store.set('voice2-resume-phrase', RESUME_DEFAULT);   // the old default migrates; a customised phrase stays
  if (store.get('voice2-start-word', null) === 'pause') store.set('voice2-start-word', START_DEFAULT);   // so does the old start word
  const kwsEnabled = () => store.get('voice2-kws', null) ?? true;   // on-device recognition is the default; the toggle in Settings turns it off
  const startWord = () => normalize(store.get('voice2-start-word', START_DEFAULT));
  const resumePhrase = () => store.get('voice2-resume-phrase', RESUME_DEFAULT);
  const commandWord = () => normalize(store.get('voice2-cmd-word', CMD_DEFAULT)) || CMD_DEFAULT;
  const cancelWord = () => { const w = normalize(store.get('voice2-cancel-word', CANCEL_WORD_DEFAULT)); return !w || w === 'bamboo' ? CANCEL_WORD_DEFAULT : w; };   // a saved "bamboo" is the old default: migrate it
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
  let lastLocalNav = null;   // {dir, ms}: a "skip forward/backward" heard on the device; the transcript may add a count
  let startWordUploads = [], emptyRun = 0, backoffUntil = 0;

  const choiceOpen = () => !!pendingChoice && Date.now() < pendingChoice.until;
  /** One gate: manual dictation, an open session, an utterance being captured or transcribed, an announcement, an open switch question. No playback starts while it is true. */
  const userSpeaking = () => ctx.dictating() || !!vc || utterancePending || Date.now() < announcingUntil || choiceOpen();
  const utteranceDone = () => { utterancePending = false; clearTimeout(utteranceTimer); if (!userSpeaking()) ctx.maybeAutoplay(); };
  const setDraft = text => { draft.value = text; ctx.autosize(); };
  const showSessionText = () => setDraft([vc.base.trim(), vc.text].filter(Boolean).join(' '));

  const setVcState = (state, label) => { $('listen').dataset.state = state; $('listen-label').textContent = label; $('listen').title = 'Voice control: ' + label; $('listen').setAttribute('aria-label', 'Voice control: ' + label); };
  const vcIdle = () => { const playing = ['playing', 'loading'].includes(player.position().status); setVcState(playing ? 'listening' : 'on', playing ? 'listening' : 'paused'); };

  const recorder = createRecorder({
    log, onClip: ctx.onClip, onLevel: ctx.onLevel, onBarge, onCommandClip,
    onSpeechOnset: () => { if (!kwsSpotter.ready()) return; for (const w of kwsSpotter.restart(0.6)) onKwsHit(w); if (vc?.mode === 'command' || choiceOpen()) for (const tag of kwsSpotter.restartCommands(0.6)) onCommandHit(tag); },
    onKwsAudio: samples => { for (const w of kwsSpotter.feed(samples)) onKwsHit(w); if (vc?.mode === 'command' || choiceOpen()) for (const tag of kwsSpotter.feedCommands(samples)) onCommandHit(tag); },
    onEarcon: kind => { player.earcon(EARCON_NOTES[kind], EARCON_NOTE_S, EARCON_GAIN[kind] ?? 0.45); try { navigator.vibrate?.(kind === 'cancel' ? [60, 40, 60] : 0); } catch { /* unsupported (iOS) */ } if (lastHitAt && Date.now() - lastHitAt < 1500) log('lat', `local hit -> ${kind} sound started in ${Date.now() - lastHitAt}ms`); },
  });

  /** Our own spoken prompt: the microphone ignores speech while it plays; a keyword still interrupts it. */
  function sayAndMute(text) { sayingUntil = Date.now() + 2500 + text.length * 70; recorder.muteFor(2500 + text.length * 70, true); player.say(text).then(ms => { sayingUntil = Date.now() + ms; recorder.muteFor(ms, true); }); }
  function interruptPrompt(by) { player.stopSay(); sayingUntil = 0; announcingUntil = 0; recorder.unmute(); log('vc', `prompt interrupted by ${by}`); }

  // ---- on-device keywords ----
  /** "attention" opens a session at once (pausing playback, no upload); "katana" from standby enters command mode; "macaroni" cancels; "tomato" ends command mode or sends. */
  function onKwsHit(word) {
    if (!voiceControl || !kwsEnabled() || ctx.dictating() || Date.now() - (kwsHitMs[word] ?? 0) < 1500) return;   // the same word twice in a row is one hit; a different word may follow at once ("attention katana")
    kwsHitMs[word] = lastHitAt = Date.now();
    if (Date.now() < sayingUntil && word !== 'TOMATO') interruptPrompt(word);   // "attention" right after a switch cuts "Now on X" short
    log('vc', `on-device keyword: ${word}`);
    if (word === 'ATTENTION' && startWord() === 'attention' && !vc && !choiceOpen()) {
      if (!lat.has('sw')) lat.begin('sw', 'startword');
      lat.stage('sw', 'kws-local');
      openSession('');
    } else if (word === 'MACARONI' && cancelWord() === 'macaroni') {
      if (vc) cancelSession(); else if (pendingChoice) cancelChoice();
    } else if (word === 'KATANA' && commandWord() === 'katana' && !vc && !choiceOpen()) {   // the command word alone enters command mode: no start word first, one sound
      openSession('', true);
      enterCommandMode('');
    } else if (word === 'TOMATO' && vc?.mode === 'command') {
      leaveCommandMode('end word on device');
    } else if (word === 'TOMATO' && vc && normalize(resumePhrase()) === 'tomato') {
      sendVoiceReplyNow('end word on device');
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
    if (!inCommand) return;
    if (tag === 'SKIP_FORWARD' || tag === 'SKIP_BACKWARD') {   // runs once now; a spoken count ("skip forward three") is added when the transcript arrives
      lastLocalNav = {dir: tag === 'SKIP_FORWARD' ? 1 : -1, ms: Date.now()};
      runCommands(tag === 'SKIP_FORWARD' ? 'skip forward' : 'skip backward'); return;   // does not set lastLocalCommandMs: the clip still goes to the server for the count
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
    setVcState('heard', 'hearing you');
  }
  async function onCommandClip(blob, meta = {}) {
    if (ctx.dictating()) return;
    if (vc) { meta.part = {blob, text: vc.mode === 'command' ? '' : null, fromCommand: vc.mode === 'command'}; (vc.parts ??= []).push(meta.part); }   // kept so a spotted end word can send the audio itself; speech in command mode is never reply text
    try { await handleCommandClip(blob, meta); } finally { if (vc && !vc.ending && !vc.timer && !vc.emptyTimer && !recorder.inUtterance()) armSessionTimer(); utterancePending = false; clearTimeout(utteranceTimer); if (!userSpeaking()) ctx.maybeAutoplay(); }
  }
  /** Every start-word clip costs a Zulip upload plus messages: cap them, and back off in a noisy room instead of exhausting the rate limit. */
  function noteStartResult(useful) {
    emptyRun = useful ? 0 : emptyRun + 1;
    if (emptyRun >= EMPTY_RUN_BACKOFF && Date.now() > backoffUntil) { backoffUntil = Date.now() + BACKOFF_MS; recorder.setBackoff(true); log('vc', `noisy room: ${emptyRun} useless clips in a row, stricter detection for 60 s`); setTimeout(() => { recorder.setBackoff(false); emptyRun = 0; }, BACKOFF_MS); }
  }
  /** Not a start word: back to standby. */
  const standby = () => { recorder.setReplyMode(false); resumeBarge(); vcIdle(); };
  async function handleCommandClip(blob, meta) {
    if (Date.now() - lastLocalCommandMs < (meta.ms ?? 0) + 800 && lastLocalCommandMs) { log('vc', 'clip already handled on-device: not sent to the server'); if (vc?.parts?.length) vc.parts.at(-1).text ||= ''; if (vc?.mode === 'command') armCommandTimer(); return; }
    log('vc', `clip ${Math.round(meta.ms ?? 0)}ms (${Math.round(meta.speechMs ?? 0)}ms speech) ${vc ? 'in reply' : pendingChoice ? 'for the question' : 'for the start word'}`);
    lat.stage('sw', 'clip-end');
    if (!vc && !pendingChoice && kwsSpotter.ready() && kwsEnabled() && Date.now() - lastHitAt > (meta.ms ?? 0) + 1500) { log('vc', 'start-word clip with no local keyword: not uploaded'); standby(); return; }   // the on-device spotter hears start words; an upload per noise clip is the biggest source of Zulip calls
    if (!vc && !pendingChoice) {
      startWordUploads = startWordUploads.filter(t => Date.now() - t < 60000);
      if (startWordUploads.length >= UPLOADS_PER_MIN || zulip.rateLimitedForMs() > 0) { log('vc', `dropped a clip without uploading (${startWordUploads.length} uploads this minute${zulip.rateLimitedForMs() ? ', Zulip rate limit' : ''})`); recorder.setReplyMode(false); vcIdle(); return; }
      startWordUploads.push(Date.now());
    }
    const listening = !!vc || choiceOpen();
    // Domain words bias reply dictation only; never a prompt that names the control words: the model echoes it back on noise. Start-word clips also get keyword spotting on the server.
    const r = await voice.stt(blob, vc && vc.mode !== 'command' && (meta.speechMs ?? 0) >= 1500 ? DOMAIN_PROMPT : undefined, !listening);
    if (!r.ok) {   // a transient failure is silent: inside a reply the clip stays as audio and goes out with the reply; the user is told what to do only once
      log('vc', `stt failed: ${r.error}`);
      if (vc?.mode === 'command') {   // 🧑 command mode with no connection gave zero feedback: say what still works (the on-device words), at most every 6 s
        if (Date.now() - (vc.toldCommandOffline ?? 0) > 6000) { vc.toldCommandOffline = Date.now(); recorder.beep('empty'); toast('No connection: command not heard. Only on-device commands work.'); sayAndMute("No connection. I can't hear that command. Say cancel, stop, or switch to."); }
      } else if (vc && !vc.toldOffline) { vc.toldOffline = true; sayAndMute("Connection lost. Keep talking; I'll send when it's back."); }
      if (vc) armSessionTimer();
      resumeBarge(); vcIdle(); return;
    }
    let heard = r.text.trim();
    { const w = heard.split(/\s+/).filter(Boolean); if (w.length >= 2 && removeControlWords(removeControlWords(w, 'tomato'), 'attention').length === 0 && w.some(x => soundsLike('tomato', [x])) && w.some(x => soundsLike('attention', [x]))) { log('vc', `dropped "${heard}": only control words, an echo of noise`); heard = ''; } }
    if (meta.part) { if (meta.part.sentAway) return; meta.part.text = heard; }   // already sent from the audio: nothing more to do with this transcript
    lat.stage('sw', 'stt-roundtrip', `relay q${r.timings?.queueMs ?? '?'} w${r.timings?.workMs ?? '?'} lag${r.timings?.eventLagMs ?? '?'}`);
    log('vc', `heard "${heard}" (speech ${Math.round(meta.speechMs ?? 0)}ms, no_speech ${r.noSpeechProb?.toFixed(2)}, logprob ${r.avgLogprob?.toFixed(2)})`);
    if (!listening && normalize(heard) && ((meta.speechMs ?? 1e9) < START_MIN_SPEECH_MS || r.noSpeechProb > START_MAX_NO_SPEECH || r.avgLogprob < START_MIN_LOGPROB)) {
      log('vc', 'rejected as noise: too short or low confidence'); standby(); return;
    }
    if (!normalize(heard)) { if (!vc) { noteStartResult(false); recorder.setReplyMode(false); resumeBarge(); } else armSessionTimer(); vcIdle(); return; }   // noise must not cancel the end-of-reply timer for good
    if (lastLocalNav && Date.now() - lastLocalNav.ms < 10000) {   // "skip forward three": the device already skipped once, add the rest
      const nav = navCommand(normalize(afterStartWord(heard) ?? heard).replace(/^katana\s+/, ''));
      if (nav && nav.dir === lastLocalNav.dir && nav.count > 1) { log('vc', `count from the transcript: ${nav.count - 1} more`); ctx.navigate({...nav, count: nav.count - 1}); lastLocalNav = null; vcIdle(); return; }
      lastLocalNav = null;
    }
    if (choiceOpen()) {   // while a switch question is open, answers are matched first, even inside a session
      const w = normalize(afterStartWord(heard) ?? heard);
      const pick = choiceAnswer(w, pendingChoice, rank);
      if (pick) { pendingChoice = null; lat.stage('switch', 'answer-heard'); closeSessionForChoice(); ctx.switchTopic(pick, true); vcIdle(); return; }   // the switch tone and "Now on X" follow; no voice state lingers
      if (NEGATIVE_RE.test(w) || CANCEL_RE.test(w)) { pendingChoice = null; closeSessionForChoice(); recorder.beep('cancel'); vcIdle(); ctx.maybeAutoplay(); return; }
      if (!vc) { log('vc', `ignored during the switch question: "${heard}"`); vcIdle(); return; }   // the start word must not open a reply while we wait for the answer
    }
    if (!vc) { onStandbyTranscript(heard, r); return; }
    const body = normalize(afterStartWord(heard) ?? heard);   // a clip that begins with the start word (the on-device spotter already opened the session) still holds what follows it
    if (vc.mode === 'command') {   // COMMAND mode: the closed list only; the end word returns to input, cancel discards
      if (stripResumePhrase(body) !== null && !body.split(' ').filter(Boolean).some(w => CANCEL_RE.test(w))) { leaveCommandMode('end word'); return; }
      runCommands(body); return;
    }
    if (CANCEL_RE.test(body)) { cancelSession(); return; }   // discards this reply and carries on
    addToSession(heard);
  }
  /** A transcript with no session open: it opens one only after the start word (or the command word, or right after a send). */
  function onStandbyTranscript(heard, r) {
    // With server keyword spotting on (r.kws is a list): it must have heard ATTENTION, and Whisper must confirm the standalone word.
    const kwsOn = Array.isArray(r.kws) && startWord() === 'attention' && Date.now() - lastReplyEndMs >= FOLLOW_UP_GRACE_MS;   // the server spots "attention" only; a follow-up right after a send needs no start word
    const commandFirst = isCommandWord(normalize(heard).split(' ')[0] ?? '');   // "katana next two": the command word alone starts command mode
    if (kwsOn && !commandFirst && !r.kws.includes('ATTENTION')) { noteStartResult(false); log('vc', `ignored: "${heard}" (keyword spotter did not hear the start word)`); standby(); return; }
    let rest = afterStartWord(heard);
    // Whisper sometimes drops the leading start word but keeps the command after it: with a spotter hit, a bare command still counts.
    const commandLike = t => { const n = normalize(t); return CANCEL_RE.test(n) || n === 'send'; };
    if (commandFirst && rest === null) rest = heard;
    if (kwsOn && rest === null && commandLike(heard)) rest = heard;
    if (kwsOn && rest !== null && afterStartWord(heard) === null && !commandLike(heard) && !commandFirst) { log('vc', `ignored: "${heard}" (transcript is not the start word)`); standby(); return; }
    const followUp = Date.now() - lastReplyEndMs < FOLLOW_UP_GRACE_MS;
    if (rest === null && followUp && commandLike(heard)) rest = heard;   // a command right after a send is still a command, never reply text
    if (rest === null && followUp) { log('vc', `follow-up right after a send: "${heard}"`); openSession(''); addToSession(heard); return; }
    if (rest === null) { noteStartResult(false); resumeBarge(); log('vc', `ignored: "${heard}" (no start word)`); toast(`Ignored (no start word): ${heard.slice(0, 60)}`); recorder.setReplyMode(false); vcIdle(); return; }
    noteStartResult(true);
    if (commandFirst && afterStartWord(heard) === null) { openSession('', true); enterCommandMode(heard.trim().split(/\s+/).slice(1).join(' ')); return; }   // "katana next two" from standby: command mode directly
    openSession(rest);
  }

  // ---- command mode ----
  function enterCommandMode(rest) {
    if (!vc) return;
    clearTimeout(vc.timer); clearTimeout(vc.emptyTimer);
    const first = vc.mode !== 'command';
    vc.mode = 'command'; vc.sawCommand = true;
    if (first) { armLocalCommands(); recorder.beep('command'); log('vc', 'command mode'); log('lat', `command mode ready ${Math.round(Date.now() - (vc.openedAt ?? Date.now()))}ms after the session opened`); }
    setVcState('command', MODE_NAMES.command + ' mode');
    toast(MODE_NAMES.command[0].toUpperCase() + MODE_NAMES.command.slice(1) + ' mode: back, next, switch to…, cancel');
    armCommandTimer();
    if (rest) runCommands(rest);
  }
  function armCommandTimer() { clearTimeout(vc.timer); vc.timer = setTimeout(() => leaveCommandMode('silence'), COMMAND_SILENCE_MS); }
  /** Back to INPUT mode with the draft kept. */
  function leaveCommandMode(why) {
    if (!vc || vc.mode !== 'command') return;
    clearTimeout(vc.timer); vc.mode = 'input'; log('vc', `command mode left (${why}); back to input`);
    recorder.beep('ready'); setVcState('dictating', 'say your reply');
    armSessionTimer();
  }
  /** Words after the command word, executed in order: "next three back one switch to human manager". Unknown words are ignored; nothing here becomes text. */
  function runCommands(text) {
    const tokens = normalize(text).split(' ').filter(Boolean).filter(t => !isCommandWord(t));
    const done = [];
    const said = [];   // what was done, in words: a bare sound never says which command it was
    const tick = (name, words = name) => { done.push(name); said.push(words); recorder.beep('tick'); };
    let navigated = false, left = false; const ignored = [];
    const finishSession = (discard = false) => { if (!vc) return; clearTimeout(vc.timer); clearTimeout(vc.emptyTimer); setDraft(discard ? vc.base : vc.base + (vc.text ? (vc.base ? ' ' : '') + vc.text : '')); ctx.saveDraft(); vc = null; recorder.setReplyMode(false); };
    while (tokens.length && vc) {
      const t = tokens[0], two = tokens.slice(0, 2).join(' ');
      if (SEND_LOG_RE.test(two) || SEND_LOG_RE.test(tokens.slice(0, 3).join(' '))) { tokens.splice(0, SEND_LOG_RE.test(two) ? 2 : 3); log('vc', 'send log by voice'); tick('log', 'sending the log'); ctx.sendLog('voice command').then(r => { if (!r?.ok) sayAndMute('Sending the log failed'); }); continue; }
      if (NEXT_AGENT_RE.test(tokens.slice(0, 4).join(' ')) || NEXT_AGENT_RE.test(tokens.slice(0, 3).join(' ')) || NEXT_AGENT_RE.test(two)) { log('vc', 'next agent with unread messages'); done.push('next agent'); finishSession(true); bargePaused = false; ctx.switchNextUnread(); left = true; break; }
      if (SWITCH_BACK_RE.test(tokens.slice(0, 3).join(' ')) || SWITCH_BACK_RE.test(two)) { log('vc', 'switch back to the previous agent'); done.push('switch back'); finishSession(true); bargePaused = false; ctx.switchBack(); left = true; break; }
      if (/^(switch|switched|switching|go)$/.test(t) && /^(to|agent|agents)$/.test(tokens[1] ?? '')) {   // Whisper often hears "switched to"; the name is free-form: it takes the rest of the utterance and ends the chain
        const name = tokens.slice(/^(agent|agents)$/.test(tokens[1]) && tokens[2] === 'to' ? 3 : 2).join(' ');
        done.push('switch'); finishSession(true); voiceSwitch(name); left = true; break;
      }
      if (CANCEL_RE.test(two) || CANCEL_RE.test(t)) { cancelSession(); return done; }
      const take = [3, 2, 1].find(n => navCommand(tokens.slice(0, n).join(' ')));   // longest prefix that is one navigation command ("next three", "previous message", "back")
      if (take) { const nav = navCommand(tokens.splice(0, take).join(' ')); ctx.navigate(nav); tick('navigate', `${nav.dir === 0 ? 'again' : nav.dir > 0 ? 'next' : 'back'}${nav.whole ? ' message' : ''}${nav.count > 1 ? ' ' + nav.count : ''}`); navigated = true; continue; }
      if (/^(stop|pause|wait)$/.test(t)) { tokens.shift(); player.pause(); tick('pause', 'paused'); continue; }
      if (/^(play|resume|continue)$/.test(t)) { tokens.shift(); tick('play', 'playing'); navigated = true; ctx.resumeAfterPause(); continue; }
      log('vc', `command mode ignored "${t}"`); tokens.shift(); ignored.push(t);
    }
    if (!done.length && ignored.length && vc) {   // heard something, understood nothing: say so and show the commands (silence would look like a hang)
      const help = 'Commands: next, back, next message, previous message, next agent, switch to NAME, stop, play, cancel.';
      log('vc', `command not recognized: "${ignored.join(' ')}"`); recorder.beep('empty'); toast(`Command not recognized: “${ignored.join(' ')}”. ${help}`); sayAndMute('Command not recognized. Say next, back, switch to, or cancel.');
    }
    if (done.length) log('vc', `commands: ${done.join(', ')}`);
    if (said.length) sayAndMute(said.join(', '));   // e.g. "next message", "back 2", "paused"
    if (navigated && vc && !left) { finishSession(); vcIdle(); left = true; }
    if (vc && !left) armCommandTimer();
    return done;
  }

  // ---- switching by voice ----
  function cancelChoice() { if (pendingChoice) { pendingChoice = null; recorder.beep('cancel'); toast('Switch cancelled'); ctx.maybeAutoplay(); } }
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
      toast('Switch to which agent?'); sayAndMute('Which agent?'); armLocalCommands(); return;
    }
    const ranked = rank(spoken);
    log('vc', `switch to "${spoken}" -> ${ranked.slice(0, 3).map(x => `${x.t.name}:${x.score.toFixed(2)}`).join(', ') || 'no match'}`);
    if (!ranked.length) { log('vc', 'no agent matches'); toast(`No agent called “${spoken}”`); sayAndMute(`No agent called ${spoken}`); return; }
    const clear = clearWinner(ranked), names = clear ? [ranked[0].t.name] : ranked.slice(0, 3).map(x => x.t.name);
    const readable = n => n.replace(/[_-]/g, ' ');
    askChoice(names, clear, clear ? `Switch to ${readable(names[0])}?` : names.map((n, i) => `${i + 1}, ${readable(n)}.`).join(' '));
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
    const speak = () => player.say(`Now on ${topic}`).then(ms => { sayingUntil = Date.now() + ms; recorder.muteFor(ms, true); announcingUntil = Date.now() + ms + 300; resumeLater(ms + 350); });
    if (confirmed) setTimeout(() => { if (ctx.current() === topic) speak(); else announcingUntil = 0; }, COMMAND_SOUND_MS + 100); else speak();
  }

  // ---- input mode: the reply session ----
  function openSession(rest, quiet = false) {
    bargePaused = false;
    if (['playing', 'loading'].includes(player.position().status)) player.pause();
    lat.finish('sw', 'pause-applied');
    vc = {base: draft.value, text: '', timer: 0, emptyTimer: 0, openedAt: Date.now()};
    recorder.setReplyMode(true);
    if (!quiet) recorder.beep('ready');   // quiet: the command sound follows
    log('vc', `session opened by start word; rest "${rest}"`);
    if (CANCEL_RE.test(normalize(rest))) { log('vc', 'cancel right after the start word'); vc = null; recorder.setReplyMode(false); recorder.beep('cancel'); ctx.resumeAfterPause(); vcIdle(); return; }
    if (rest) addToSession(rest);
    else { setVcState('dictating', 'say your reply'); vc.emptyTimer = setTimeout(closeEmptySession, EMPTY_SESSION_MS); }
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
  function addToSession(heard) {
    if (hasCancel(heard, cancelWord())) { cancelSession(); return; }   // the rare cancel word fires wherever it is said
    const words = (afterStartWord(heard) ?? heard).split(/\s+/).filter(Boolean), at = words.findIndex(isCommandWord);
    if (at >= 0 && vc && vc.mode !== 'command') {   // in input mode the command word is not a command
      log('vc', 'command word inside dictation: ignored, not a mode switch'); words.splice(at, 1); heard = words.join(' ');
      if (!heard.trim()) { armSessionTimer(); return; }
    } else if (at >= 0 && vc) {   // in command mode: what precedes the command word is reply text, what follows is commands
      const before = words.slice(0, at).join(' ');
      if (before) { vc.text = [vc.text, withoutStrayEndWords(before)].filter(Boolean).join(' '); showSessionText(); }
      enterCommandMode(words.slice(at + 1).join(' ')); return;
    }
    if (endsCancel(heard, cancelWord())) { cancelSession(); return; }   // "... cancel" at the end of any utterance cancels the whole reply
    clearTimeout(vc.emptyTimer); vc.emptyTimer = 0;
    if (stripResumePhrase(heard) !== null && !vc.sending) { vc.text = [vc.text, (afterStartWord(heard) ?? heard).trim()].filter(Boolean).join(' '); showSessionText(); sendVoiceReplyNow('end word in the transcript'); return; }   // no quiet window
    vc.text = [vc.text, withoutStrayEndWords((afterStartWord(heard) ?? heard).trim())].filter(Boolean).join(' ');
    showSessionText();
    setVcState('dictating', 'dictating reply');
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
    if (utterancePending) { vc.emptyTimer = setTimeout(closeEmptySession, 2000); return; }   // still talking (the clip's transcript is not back yet): never close mid-speech
    if (untranscribed()) { sendVoiceReplyNow('clips could not be transcribed here'); return; }   // offline: the audio is sent and transcribed by the server later
    log('vc', 'empty session timed out: resuming');
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
    if (why.includes('on device')) { const w = reply.text.trim().split(/\s+/); if (w.length && looksLikeClippedEndWord(w.at(-1))) { log('vc', `dropped the clipped end word "${w.at(-1)}"`); reply.text = w.slice(0, -1).join(' '); } }
    const items = [];
    if (reply.base.trim()) items.push({text: reply.base.trim()});
    if (reply.text.trim()) items.push({text: reply.text.trim()});   // everything already transcribed, in arrival order
    for (const p of reply.parts ?? []) { p.sentAway = true; if (p.text === null && !reply.sawCommand) items.push({blob: p.blob}); }   // clips still waiting for text go as audio (the relay joins them); after a command word the audio may hold commands: only text is trusted
    vc = null; recorder.setReplyMode(false); lastReplyEndMs = Date.now();
    log('vc', `${why}: sending now (${items.filter(i => i.blob).length} clips for the server to transcribe)`);
    if (!items.some(i => i.blob)) {   // the relay applies the same rules; nothing is even uploaded
      const asm = assembleReply(items.map(i => i.text ?? ''), {cancelWord: cancelWord(), commandWord: commandWord()});
      if (asm.discard || !asm.text) { log('vc', `reply not sent (${asm.why ?? 'only control words'})`); setDraft(reply.base); ctx.saveDraft(); recorder.beep('cancel'); ctx.resumeAfterPause(); vcIdle(); return; }
    }
    recorder.beep('done');
    ctx.sendLocal(reply.text || '…', items);
    setDraft(''); ctx.saveDraft();
    setVcState('sending', 'sending');
    player.unlock(); ctx.resumeAfterPause(); vcIdle();
  }
  /** End word plus silence (or the long silence): drop the end word, send the reply if there is one, then resume reading. */
  function finishVcReply() {
    if (!vc || vc.ending) return;
    if (recorder.inUtterance()) { clearTimeout(vc.timer); vc.timer = setTimeout(finishVcReply, 1500); log('vc', 'end timer fired while you were still speaking: waiting'); return; }   // 🧑 "A buggy early stop, although I did not say the terminal word": the timer armed by an earlier transcript fired in the middle of a long clip   // a timer re-armed while a send was waiting for clips must not fire on a closed session
    if (untranscribed()) { sendVoiceReplyNow('clips still untranscribed'); return; }
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
    if (!r.ok && voiceControl) { toast('On-device start word failed to load; using the server (slower)'); setTimeout(() => $('set-kws').checked = kwsEnabled(), 0); }
  }
  async function setVoiceControl(on) {
    voiceControl = on; voiceListening = on; store.set('voice2-vc', on);
    $('set-voice').checked = on; if (on) vcIdle(); else setVcState('off', 'off');
    if (!on) { recorder.listen(false); kwsSpotter.unload(); return; }
    if (kwsEnabled()) loadKws(); else log('kws', 'on-device start word is OFF in Settings: start words take the server path (slower)');   // lazy: the model (about 21 MB over the wire, once) loads only when voice control is switched on
    const r = await recorder.listen(true);
    if (r.ok && !r.running) { voiceListening = false; setVcState('waiting', 'tap'); log('vc', 'armed, waiting for a tap'); return; }
    if (!r.ok) { toast(r.error); player.say('Microphone stopped. Tap the microphone button to restart.'); voiceControl = false; store.set('voice2-vc', false); $('set-voice').checked = false; setVcState('off', 'off'); }
    log('vc', `voice control ${r.ok ? 'on' : 'failed'}`);
    if (r.ok && r.running) recorder.beep('ready');
  }

  // ---- settings sheet ----
  document.querySelectorAll('.mn').forEach(e => { e.textContent = MODE_NAMES[e.dataset.m]; });   // help text uses the names above
  const textSetting = (id, key, fallback, clean) => { $(id).value = store.get(key, fallback); $(id).onchange = e => store.set(key, clean(e.target.value.trim()) || fallback); };
  textSetting('set-phrase', 'voice2-resume-phrase', RESUME_DEFAULT, v => v);
  textSetting('set-start', 'voice2-start-word', START_DEFAULT, v => v.toLowerCase());
  textSetting('set-cancel', 'voice2-cancel-word', CANCEL_WORD_DEFAULT, v => v.toLowerCase());
  textSetting('set-cmd', 'voice2-cmd-word', CMD_DEFAULT, v => v.toLowerCase());
  $('set-silence').value = store.get('voice2-long-silence-s', LONG_SILENCE_DEFAULT_S);
  $('set-silence').onchange = e => store.set('voice2-long-silence-s', Math.max(0, Number(e.target.value) || 0));
  $('set-kws').checked = kwsEnabled();
  $('set-kws').onchange = e => { store.set('voice2-kws', e.target.checked); if (e.target.checked && voiceControl) loadKws(); else if (!e.target.checked) kwsSpotter.unload(); };
  $('set-voice').onchange = e => setVoiceControl(e.target.checked);
  $('listen').onclick = e => { e.stopPropagation(); setVoiceControl(!voiceControl); toast(voiceControl ? 'Voice control on: say attention, then your reply, and finish with "tomato"' : 'Voice control off'); };

  return {
    recorder, userSpeaking, announceSwitch, prebuildCommands, cancelWord, commandWord,
    enabled: () => voiceControl,
    sessionOpen: () => !!vc,
    /** A saved "on" re-arms after reload: try at once, else on the first tap anywhere. */
    start() { if (voiceControl) { $('set-voice').checked = true; setVcState('waiting', 'tap'); setVoiceControl(true); } },
    onGesture() { if (voiceControl && !recorder.active() && !voiceListening) setVoiceControl(true); },
    /** The draft was sent by hand: a later timer must not send it again. */
    soundLeftMs: () => recorder.earconLeftMs(),
    closeForManualSend() { if (vc) { clearTimeout(vc.timer); clearTimeout(vc.emptyTimer); vc = null; recorder.setReplyMode(false); log('vc', 'draft sent by hand: voice session closed'); } },
    debugLine: () => `on-device start word: setting ${kwsEnabled() ? 'on' : 'OFF'}, ${kwsSpotter.state().status}${kwsSpotter.state().detail ? ' (' + kwsSpotter.state().detail + ')' : ''}`,
  };
}
