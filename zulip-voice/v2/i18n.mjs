/** Interface language: a small message catalog (en, zh), `t(key, params)`, and DOM helpers. The voice keywords and command grammar are English in both languages. */
import {store} from './store.mjs';

export const LANG_KEY = 'voice2-lang';
const LANGS = ['en', 'zh'];
const browserLang = () => (globalThis.navigator?.languages?.[0] ?? globalThis.navigator?.language ?? 'en').toLowerCase().startsWith('zh') ? 'zh' : 'en';
let lang = LANGS.includes(store.get(LANG_KEY, null)) ? store.get(LANG_KEY, null) : browserLang();
const listeners = [];

/** Catalog values: a string with `{name}` slots, or a function of the params (plurals). */
const en = {
  title: 'Voice', conversation: 'Conversation', jump: 'Jump to playing',
  'first-unread': 'Scroll to first unread message',
  'net.off': 'No connection. Messages and audio will resume by themselves.',
  'net.limited': p => `Zulip is rate limiting this account; resuming in ${p.s}s.`,
  'conn.offline': 'Offline', 'conn.reconnecting': 'Reconnecting…',
  'agent.choose': 'Choose agent', 'agent.chooseAria': 'Choose agent', 'agent.default': 'Agent',
  'conn.aria': 'Connection and debug', settings: 'Settings',
  prev: 'Previous message', next: 'Next message', play: 'Play', pause: 'Pause', 'find.aria': 'Switch agent',
  slower: 'Slower', faster: 'Faster', auto: 'Auto', message: 'Message',
  'cancel-rec': 'Discard recording', 'next-unread': 'Next agent with unread messages', 'prev-agent': 'Back to the previous agent',
  'prev-agent.title': 'Back to {name}', attach: 'Attach a file or photo',
  'vc.aria': 'Voice control: {label}', dictate: 'Dictate', 'dictate.stop': 'Stop dictation', send: 'Send', 'send.finish': 'Finish and send',
  'search.ph': 'Search agents', close: 'Close',
  'keys.title': 'Keyboard shortcuts',
  'k.space': 'play / pause', 'k.arrows': 'previous / next chunk', 'k.shiftarrows': 'previous / next message', 'k.speed': 'slower / faster',
  'k.a': 'auto-read on / off', 'k.m': 'dictate; again: stop and keep text', 'k.esc': 'discard recording; leave the text box', 'k.enter': 'send (in the text box)',
  'k.i': 'type a reply', 'k.slash': 'find agent', 'k.alt': 'previous / next agent in the list', 'k.v': 'voice control on / off', 'k.q': 'this list',
  'set.title': 'Settings', 'set.language': 'Language', 'set.autoplay': 'Autoplay new messages', 'set.voice': 'Voice control',
  'set.phrase': 'End phrase (default: tomato; to say the word itself, say "roo" or type it)',
  'set.help': `<b><span class="mn" data-m="input"></span> mode:</b> say the start word ("attention"), then talk: everything you say is your reply. Finish it with <b>tomato</b> (sends). Say <b>tornado</b> anywhere, in any mode, to cancel and discard at once (the old <b>cancel</b> still works through the server).<br><b><span class="mn" data-m="command"></span> mode:</b> say the command word (default <b>potato</b>; a double thump) from standby on its own: no start word is needed. While input mode is open (including right after "attention") it does nothing. Now only commands are heard, nothing becomes text, and each one answers with a tick. They chain in one breath. Commands: <b>skip forward</b> / <b>skip backward</b> (add a count: <b>skip forward three</b>; up to ten; past the last chunk it ends reading), <b>previous message</b>, <b>next message</b>, <b>first unread</b> (or <b>unread</b>: scroll to the first unread message of this agent; playback is untouched), <b>again</b>, <b>stop</b>, <b>play</b>, <b>switch to NAME</b>, <b>switch back</b> (old <b>next</b>/<b>back</b> still work). Confirming a switch (<b>positive</b>) answers at once with the same double thump; then the new agent's unread messages start, or, if there are none, "Now on NAME" is spoken (never over a message); a falling pair means it was declined, cancelled or timed out. Answers to "Switch to NAME?": <b>positive</b> / <b>negative</b> (or yes / no), or <b>number one</b>, <b>number two</b>… Navigation and switching leave command mode; four seconds of quiet, or <b>tomato</b>, return to input mode with your draft kept.`,
  'set.cancel': 'Cancel word (default: tornado; discards what you are saying, anywhere)',
  'set.cancel.note': 'Only the default cancel word is also recognised on the phone itself (instant). A different word is recognised by the server after you stop talking: slower.',
  'set.cmd': 'Command word (default: potato)', 'set.start': 'Start word (default: attention)',
  'set.silence': 'Long silence finishes a reply (seconds, 0 = off)',
  'set.kws': 'On-device keyword recognition (on by default; uses about 300 MB more memory, turn off if the page reloads on its own)',
  'set.switchConfirm': 'Ask "Switch to X?" before a voice switch with one clear match (off: switch at once; say switch again if it is wrong)',
  'set.hideAtt': 'Hide attachments', 'set.speed': 'Speed',
  'open-keys': 'Keyboard shortcuts (?)', 'open-debug': 'Connection and debug log', reload: 'Reload app', 'sign-out': 'Sign out…',
  'sign-out.confirm': 'Sign out of this account?',
  'debug.title': 'Debug', 'dbg.copy': 'Copy all', 'dbg.send': 'Send to server', 'dbg.ping': 'Test voice', 'dbg.kick': 'Reconnect', 'dbg.clear': 'Clear', 'dbg.self': 'Self-test',
  'login.aria': 'Sign in', 'login.howto': 'New here? How to use this', 'login.note': 'Paste your zuliprc once. The key stays in this browser.', 'login.connect': 'Connect',
  'login.need': 'Need [api] with site, email and key.', 'login.bot': 'Use the human account, not a bot.',
  // feed and view
  'feed.earlier': 'Load earlier messages', 'feed.choose': 'Choose an agent', 'feed.loading': 'Loading…', 'feed.none': 'No messages yet',
  'msg.switching': 'Switching to {name}', 'msg.empty': '(empty)', 'msg.items': p => `· ${p.n} item${p.n === 1 ? '' : 's'}`, 'msg.markRead': 'Mark read',
  'att.tapShow': ' · tap to show', 'att.hide': 'Hide', 'out.sending': 'Sending…', 'out.queued': 'Waiting for connection…', 'out.failed': 'Not sent', 'out.retry': 'Retry', 'out.delete': 'Delete',
  'you.prefix': 'You: ',
  'ago.now': 'now', 'ago.m': '{n}m', 'ago.h': '{n}h', 'ago.d': '{n}d',
  'media.title': 'Voice', 'media.artist': 'Agent', 'media.album': 'Zulip voice',
  // toasts and banners
  'toast.markedRead': 'Marked read', 'toast.tapListen': 'Tap to listen', 'toast.tapListenN': 'Tap to listen ({n} unread)', 'toast.end': 'End of messages',
  'toast.noAgentMsgs': 'No agent messages here', 'toast.noEarlier': 'No earlier messages', 'toast.loadingAudio': 'Loading audio…', 'toast.waitConn': 'Waiting for the connection…',
  'toast.stillLoading': 'Still trying to load audio…', 'toast.nothingRead': 'Nothing to read',
  'banner.waitAudio': 'Waiting for audio ({error}). Retrying by itself…', 'banner.resume': 'Tap to resume audio',
  'toast.noOther': 'No other agent', 'toast.noUnreadElsewhere': 'No unread messages elsewhere: next agent in the list', 'toast.noPrev': 'No previous agent',
  'toast.notFound': 'Agent “{name}” not found', 'toast.switched': 'Switched to {name}', 'toast.back': 'Back',
  'toast.uploading': p => `Uploading ${p.n} file${p.n > 1 ? 's' : ''}…`, 'toast.uploadFailed': 'Upload failed: {name} ({error})',
  'toast.attached1': 'File attached: tap send', 'toast.attachedN': 'Files attached: tap send', 'toast.noUnreadHere': 'No unread messages here',
  'toast.cancelled': 'Reply cancelled', 'toast.nothingRecognised': 'Nothing was recognised in that recording',
  'toast.copied': 'Copied', 'toast.selectAll': 'Select-all done; use Copy in the menu', 'toast.sent': 'Sent to server', 'toast.sendFailed': 'Send failed',
  'banner.update': 'New version available. Tap to reload.', 'toast.rateDelay': 'Zulip is rate limiting: sending is delayed ({s}s). Nothing is lost.',
  // errors
  'err.upload': 'upload failed', 'err.clipExpired': 'clip expired', 'err.timeout': '{op} timed out', 'err.rateLimited': 'rate limited',
  'err.micDenied': 'Microphone permission denied.', 'err.micDeniedSafari': 'Microphone permission denied. Allow it in Settings > Safari > Microphone.', 'err.micUnavailable': 'Microphone unavailable ({name}).',
  // dictation
  'dict.voiceDown': 'Voice service is not responding; dictation may fail', 'dict.slowMic': 'Microphone is slow to start; wait a moment before speaking', 'dict.starting': 'Starting microphone…',
  'dict.failedClips': p => `${p.n} dictation clip${p.n > 1 ? 's' : ''} not transcribed. Tap to retry`,
  // voice control
  'mode.standby': 'standby', 'mode.input': 'input', 'mode.command': 'command', 'mode.commandTitle': 'Command mode', 'mode.inputTitle': 'Input mode',
  'mode.commandHint': 'Still listening. Say a command: next, back, pause, resume, switch to NAME. "{resume}" leaves, "{cancel}" cancels. Two equal low beats = still in command mode.',
  'mode.inputHint': 'Still listening. What you say becomes your reply. "{resume}" sends, "{cancel}" cancels. A low rising beat = still listening.',
  'vc.off': 'off', 'vc.tap': 'tap', 'vc.listening': 'listening', 'vc.paused': 'paused', 'vc.hearing': 'hearing you', 'vc.sayReply': 'say your reply', 'vc.dictating': 'dictating reply', 'vc.sending': 'sending', 'vc.command': 'command mode',
  'vc.on': 'Voice control on: say attention, then your reply, and finish with "tomato"', 'vc.offToast': 'Voice control off',
  'vc.noConnToast': 'No connection: command not heard. Only on-device commands work.',
  'vc.ignored': 'Ignored (no start word): {heard}',
  'vc.commandToast': 'Command mode: back, next, switch to…, cancel',
  'vc.notCommandToast': 'Not a command, kept in your reply: “{kept}”. Say {resume} to go back to input.',
  'vc.unrecognizedToast': 'Command not recognized: “{heard}”. Commands: next, back, restart, next message, previous message, first unread, next agent, switch to NAME, stop, play, cancel.',
  'vc.switchCancelled': 'Switch cancelled', 'vc.whichToast': 'Switch to which agent?', 'vc.noAgentToast': 'No agent called “{name}”',
  'vc.kwsFailed': 'On-device start word failed to load; using the server (slower)',
  'sep': ', ',
  'say.noConnCmd': "No connection. I can't hear that command. Say cancel, stop, or switch to.",
  'say.connLost': "Connection lost. Keep talking; I'll send when it's back.",
  'say.notCommand': 'Not a command. Kept in your reply.', 'say.unrecognized': 'Command not recognized. Say next, back, switch to, or cancel.',
  'say.which': 'Which agent?', 'say.noAgent': 'No agent called {name}', 'say.switchTo': 'Switch to {name}?', 'say.choice': '{n}, {name}.', 'say.nowOn': 'Now on {name}',
  'say.micStopped': 'Microphone stopped. Tap the microphone button to restart.', 'say.logFailed': 'Sending the log failed',
  'say.sendingLog': 'sending the log', 'say.paused': 'paused', 'say.playing': 'playing',
  'nav.unread': 'first unread', 'nav.restart': 'from the start', 'nav.again': 'again', 'nav.next': 'next', 'nav.back': 'back', 'nav.nextMsg': 'next message', 'nav.backMsg': 'back message',
};

const zh = {
  title: '语音', conversation: '对话', jump: '跳到正在播放',
  'first-unread': '滚动到第一条未读消息',
  'net.off': '没有网络连接。恢复后消息和音频会自动继续。',
  'net.limited': p => `Zulip 正在限制此账号的请求频率；${p.s} 秒后恢复。`,
  'conn.offline': '离线', 'conn.reconnecting': '重新连接中…',
  'agent.choose': '选择智能体', 'agent.chooseAria': '选择智能体', 'agent.default': '智能体',
  'conn.aria': '连接与调试', settings: '设置',
  prev: '上一条消息', next: '下一条消息', play: '播放', pause: '暂停', 'find.aria': '切换智能体',
  slower: '减速', faster: '加速', auto: '自动', message: '消息',
  'cancel-rec': '放弃录音', 'next-unread': '下一个有未读消息的智能体', 'prev-agent': '返回上一个智能体',
  'prev-agent.title': '返回 {name}', attach: '添加文件或照片',
  'vc.aria': '语音控制：{label}', dictate: '听写', 'dictate.stop': '停止听写', send: '发送', 'send.finish': '完成并发送',
  'search.ph': '搜索智能体', close: '关闭',
  'keys.title': '键盘快捷键',
  'k.space': '播放 / 暂停', 'k.arrows': '上一段 / 下一段', 'k.shiftarrows': '上一条 / 下一条消息', 'k.speed': '减速 / 加速',
  'k.a': '自动朗读 开 / 关', 'k.m': '听写；再按：停止并保留文字', 'k.esc': '放弃录音；离开输入框', 'k.enter': '发送（在输入框中）',
  'k.i': '输入回复', 'k.slash': '查找智能体', 'k.alt': '列表中的上一个 / 下一个智能体', 'k.v': '语音控制 开 / 关', 'k.q': '显示此列表',
  'set.title': '设置', 'set.language': '语言', 'set.autoplay': '自动播放新消息', 'set.voice': '语音控制',
  'set.phrase': '结束词（默认：tomato；若要说出这个词本身，请说 "roo" 或手动输入）',
  'set.help': `<b><span class="mn" data-m="input"></span>模式：</b>说出开始词（"attention"），然后说话：你说的一切都是回复。以 <b>tomato</b> 结束（发送）。在任何模式下的任何时候说 <b>tornado</b>，立即取消并丢弃（旧的 <b>cancel</b> 仍可通过服务器使用）。<br><b><span class="mn" data-m="command"></span>模式：</b>在待机状态下直接说命令词（默认 <b>potato</b>；一次双击声），不需要开始词。输入模式开启期间（包括刚说完 "attention" 之后）它不起作用。此后只听命令，不会变成文字，每条命令都会以一声提示音回应，可以连着说。命令：<b>skip forward</b> / <b>skip backward</b>（可加数字：<b>skip forward three</b>；最多十；超过最后一段则结束朗读）、<b>previous message</b>、<b>next message</b>、<b>first unread</b>（或 <b>unread</b>：滚动到此智能体的第一条未读消息；播放不受影响）、<b>again</b>、<b>stop</b>、<b>play</b>、<b>switch to NAME</b>、<b>switch back</b>（旧的 <b>next</b>/<b>back</b> 仍可用）。确认切换（<b>positive</b>）会立即以同样的双击声回应；然后开始播放新智能体的未读消息，若没有未读消息，则朗读“现在在 NAME”（不会打断消息）；下降的两声表示被拒绝、取消或超时。回答 "Switch to NAME?"：<b>positive</b> / <b>negative</b>（或 yes / no），或 <b>number one</b>、<b>number two</b>……导航和切换会退出命令模式；安静四秒，或说 <b>tomato</b>，回到输入模式并保留草稿。语音关键词和命令始终使用英文。`,
  'set.cancel': '取消词（默认：tornado；在任何位置丢弃你正在说的内容）',
  'set.cancel.note': '只有默认的取消词也会在手机本地识别（即时）。其他词由服务器在你停止说话后识别：较慢。',
  'set.cmd': '命令词（默认：potato）', 'set.start': '开始词（默认：attention）',
  'set.silence': '长时间静音结束回复（秒，0 = 关闭）',
  'set.kws': '设备端关键词识别（默认开启；约多占用 300 MB 内存，如果页面自行重新加载请关闭）',
  'set.switchConfirm': '语音切换有一个明确匹配时，先询问 "Switch to X?"（关闭：立即切换；切错了再说 switch）',
  'set.hideAtt': '隐藏附件', 'set.speed': '语速',
  'open-keys': '键盘快捷键 (?)', 'open-debug': '连接与调试日志', reload: '重新加载应用', 'sign-out': '退出登录…',
  'sign-out.confirm': '退出此账号？',
  'debug.title': '调试', 'dbg.copy': '全部复制', 'dbg.send': '发送到服务器', 'dbg.ping': '测试语音', 'dbg.kick': '重新连接', 'dbg.clear': '清除', 'dbg.self': '自检',
  'login.aria': '登录', 'login.howto': '第一次使用？使用说明', 'login.note': '粘贴一次 zuliprc。密钥只保存在此浏览器中。', 'login.connect': '连接',
  'login.need': '需要包含 site、email 和 key 的 [api] 段。', 'login.bot': '请使用人类账号，不要使用机器人账号。',
  'feed.earlier': '加载更早的消息', 'feed.choose': '请选择智能体', 'feed.loading': '加载中…', 'feed.none': '还没有消息',
  'msg.switching': '正在切换到 {name}', 'msg.empty': '（空）', 'msg.items': p => `· ${p.n} 项`, 'msg.markRead': '标为已读',
  'att.tapShow': ' · 点按显示', 'att.hide': '隐藏', 'out.sending': '发送中…', 'out.queued': '等待连接…', 'out.failed': '未发送', 'out.retry': '重试', 'out.delete': '删除',
  'you.prefix': '你：',
  'ago.now': '刚刚', 'ago.m': '{n}分钟', 'ago.h': '{n}小时', 'ago.d': '{n}天',
  'media.title': '语音', 'media.artist': '智能体', 'media.album': 'Zulip 语音',
  'toast.markedRead': '已标为已读', 'toast.tapListen': '点按开始收听', 'toast.tapListenN': '点按开始收听（{n} 条未读）', 'toast.end': '消息已读完',
  'toast.noAgentMsgs': '这里没有智能体的消息', 'toast.noEarlier': '没有更早的消息', 'toast.loadingAudio': '正在加载音频…', 'toast.waitConn': '正在等待连接…',
  'toast.stillLoading': '仍在尝试加载音频…', 'toast.nothingRead': '没有可朗读的内容',
  'banner.waitAudio': '正在等待音频（{error}）。将自动重试…', 'banner.resume': '点按恢复音频',
  'toast.noOther': '没有其他智能体', 'toast.noUnreadElsewhere': '其他地方没有未读消息：切换到列表中的下一个智能体', 'toast.noPrev': '没有上一个智能体',
  'toast.notFound': '找不到智能体“{name}”', 'toast.switched': '已切换到 {name}', 'toast.back': '返回',
  'toast.uploading': p => `正在上传 ${p.n} 个文件…`, 'toast.uploadFailed': '上传失败：{name}（{error}）',
  'toast.attached1': '文件已添加：点按发送', 'toast.attachedN': '文件已添加：点按发送', 'toast.noUnreadHere': '这里没有未读消息',
  'toast.cancelled': '回复已取消', 'toast.nothingRecognised': '这段录音中没有识别到内容',
  'toast.copied': '已复制', 'toast.selectAll': '已全选；请使用菜单中的复制', 'toast.sent': '已发送到服务器', 'toast.sendFailed': '发送失败',
  'banner.update': '有新版本。点按重新加载。', 'toast.rateDelay': 'Zulip 正在限制请求频率：发送延迟（{s} 秒）。不会丢失内容。',
  'err.upload': '上传失败', 'err.clipExpired': '录音已过期', 'err.timeout': '{op} 超时', 'err.rateLimited': '请求过于频繁',
  'err.micDenied': '麦克风权限被拒绝。', 'err.micDeniedSafari': '麦克风权限被拒绝。请在 设置 > Safari > 麦克风 中允许。', 'err.micUnavailable': '麦克风不可用（{name}）。',
  'dict.voiceDown': '语音服务没有响应；听写可能失败', 'dict.slowMic': '麦克风启动较慢；请稍等片刻再说话', 'dict.starting': '正在启动麦克风…',
  'dict.failedClips': p => `${p.n} 段听写录音未能转写。点按重试`,
  'mode.standby': '待机', 'mode.input': '输入', 'mode.command': '命令', 'mode.commandTitle': '命令模式', 'mode.inputTitle': '输入模式',
  'mode.commandHint': '仍在聆听。请说命令：next、back、pause、resume、switch to NAME。"{resume}" 退出，"{cancel}" 取消。两声相同的低音 = 仍在命令模式。',
  'mode.inputHint': '仍在聆听。你说的内容会成为回复。"{resume}" 发送，"{cancel}" 取消。一声低而上升的提示音 = 仍在聆听。',
  'vc.off': '关闭', 'vc.tap': '点按', 'vc.listening': '聆听中', 'vc.paused': '已暂停', 'vc.hearing': '正在听你说', 'vc.sayReply': '请说出回复', 'vc.dictating': '正在听写回复', 'vc.sending': '发送中', 'vc.command': '命令模式',
  'vc.on': '语音控制已开启：说 attention，然后说出回复，以 "tomato" 结束', 'vc.offToast': '语音控制已关闭',
  'vc.noConnToast': '没有网络：没有听到命令。只有设备端命令可用。',
  'vc.ignored': '已忽略（没有开始词）：{heard}',
  'vc.commandToast': '命令模式：back、next、switch to…、cancel',
  'vc.notCommandToast': '不是命令，已保留在你的回复中：“{kept}”。说 {resume} 回到输入模式。',
  'vc.unrecognizedToast': '没有识别到命令：“{heard}”。命令：next、back、restart、next message、previous message、first unread、next agent、switch to NAME、stop、play、cancel。',
  'vc.switchCancelled': '已取消切换', 'vc.whichToast': '切换到哪个智能体？', 'vc.noAgentToast': '没有名为“{name}”的智能体',
  'vc.kwsFailed': '设备端开始词加载失败；改用服务器（较慢）',
  'sep': '，',
  'say.noConnCmd': '没有网络连接，听不到这条命令。请说 cancel、stop 或 switch to。',
  'say.connLost': '连接已断开。请继续说，恢复后我会发送。',
  'say.notCommand': '不是命令。已保留在你的回复中。', 'say.unrecognized': '没有识别到命令。请说 next、back、switch to 或 cancel。',
  'say.which': '哪个智能体？', 'say.noAgent': '没有名为 {name} 的智能体', 'say.switchTo': '切换到 {name}？', 'say.choice': '{n}，{name}。', 'say.nowOn': '现在在 {name}',
  'say.micStopped': '麦克风已停止。点按麦克风按钮重新开始。', 'say.logFailed': '发送日志失败',
  'say.sendingLog': '发送日志', 'say.paused': '已暂停', 'say.playing': '播放',
  'nav.unread': '第一条未读', 'nav.restart': '从头开始', 'nav.again': '重播', 'nav.next': '前进', 'nav.back': '后退', 'nav.nextMsg': '下一条消息', 'nav.backMsg': '上一条消息',
};
const tables = {en, zh};

export const getLang = () => lang;
export const speechLang = () => (lang === 'zh' ? 'zh-CN' : 'en-US');

/** The text for `key` in the current language (English if missing, the key if absent everywhere). `{name}` slots take `params`. */
export function t(key, params = {}) {
  const v = tables[lang][key] ?? en[key] ?? key;
  return typeof v === 'function' ? v(params) : v.replace(/\{(\w+)\}/g, (m, k) => params[k] ?? m);
}

/** Fills every `data-i18n*` element: text, html, aria-label, title, placeholder. */
export function applyStatic(root = document) {
  document.documentElement.lang = lang === 'zh' ? 'zh-CN' : 'en';
  for (const e of root.querySelectorAll('[data-i18n]')) e.textContent = t(e.dataset.i18n);
  for (const e of root.querySelectorAll('[data-i18n-html]')) e.innerHTML = t(e.dataset.i18nHtml);
  for (const e of root.querySelectorAll('[data-i18n-aria]')) e.setAttribute('aria-label', t(e.dataset.i18nAria));
  for (const e of root.querySelectorAll('[data-i18n-title]')) e.title = t(e.dataset.i18nTitle);
  for (const e of root.querySelectorAll('[data-i18n-ph]')) e.placeholder = t(e.dataset.i18nPh);
  for (const e of root.querySelectorAll('.mn')) e.textContent = t('mode.' + e.dataset.m);
  document.title = t('title');
}

/** Calls `fn` after every language change (to redraw text the page produced itself). */
export const onLangChange = fn => { listeners.push(fn); };
export function setLang(next) {
  if (!LANGS.includes(next) || next === lang) return;
  lang = next; store.set(LANG_KEY, next); applyStatic();
  for (const fn of listeners) fn(next);
}
export const LANGUAGES = [['en', 'English'], ['zh', '中文']];
