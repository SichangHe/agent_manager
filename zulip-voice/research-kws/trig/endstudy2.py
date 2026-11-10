"""End-word study 2: candidates vs the kangaroo baseline. Positive: word at the end of a sentence under 7 conditions x 3 carriers (KWS hit and transcript hit).
False accepts: KWS over 35 min of agent speech and over TTS of the human's own Zulip messages; transcript occurrences in 55k words of the human's text."""
import json, re, subprocess, sys, urllib.request
import numpy as np, soundfile as sf, sherpa_onnx
M = "../sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01"
WORDS = ["kangaroo", "terminate", "affirmative", "transmission", "conclusion", "dismissed", "execute", "tornado", "watermelon", "attention"]
CARRIERS = ["Please run the build on staging and tell me how it went", "I think we should compare the results with the previous runs", "Yes, go ahead and deploy it to the second machine"]
rng = np.random.default_rng(2)
def ff(x, sr, af):
    p = subprocess.run(["ffmpeg", "-nostdin", "-v", "error", "-f", "f32le", "-ar", str(sr), "-ac", "1", "-i", "pipe:0", "-af", af, "-ar", "16000", "-f", "f32le", "pipe:1"], input=x.astype(np.float32).tobytes(), capture_output=True)
    return np.frombuffer(p.stdout, dtype=np.float32)
def tts(text):
    r = urllib.request.urlopen(urllib.request.Request("http://127.0.0.1:8770/tts", data=json.dumps({"text": text}).encode(), headers={"Content-Type": "application/json"}), timeout=120).read()
    open("/tmp/_t2.wav", "wb").write(r); return sf.read("/tmp/_t2.wav", dtype="float32")
def spotter(word):
    open("e2_raw.txt", "w").write(word.upper() + "\n")
    subprocess.run(["../.venv/bin/sherpa-onnx-cli", "text2token", "--tokens", f"{M}/tokens.txt", "--tokens-type", "bpe", "--bpe-model", f"{M}/bpe.model", "e2_raw.txt", "e2_tok.txt"], check=True, capture_output=True)
    c = "chunk-16-left-64.int8.onnx"
    return sherpa_onnx.KeywordSpotter(tokens=f"{M}/tokens.txt", encoder=f"{M}/encoder-epoch-12-avg-2-{c}", decoder=f"{M}/decoder-epoch-12-avg-2-{c}", joiner=f"{M}/joiner-epoch-12-avg-2-{c}", keywords_file="e2_tok.txt", keywords_threshold=0.25, keywords_score=1.0, num_threads=1, provider="cpu")
def run(sp, x):
    s = sp.create_stream(); hits = 0
    for i in range(0, len(x), 1600):
        s.accept_waveform(16000, x[i:i + 1600])
        while sp.is_ready(s):
            sp.decode_stream(s)
            if sp.get_result(s): hits += 1; sp.reset_stream(s)
    s.accept_waveform(16000, np.zeros(8000, dtype=np.float32)); s.input_finished()
    while sp.is_ready(s):
        sp.decode_stream(s)
        if sp.get_result(s): hits += 1; sp.reset_stream(s)
    return hits
def asr(x):
    pcm = (np.clip(x, -1, 1) * 32767).astype("<i2").tobytes(); hdr = b"RIFF" + (36 + len(pcm)).to_bytes(4, "little") + b"WAVEfmt " + (16).to_bytes(4, "little") + (1).to_bytes(2, "little") + (1).to_bytes(2, "little") + (16000).to_bytes(4, "little") + (32000).to_bytes(4, "little") + (2).to_bytes(2, "little") + (16).to_bytes(2, "little") + b"data" + len(pcm).to_bytes(4, "little")
    return json.loads(urllib.request.urlopen(urllib.request.Request("http://127.0.0.1:8771/stt", data=hdr + pcm, method="POST"), timeout=120).read())["text"]
norm = lambda t: re.sub(r"[^a-z0-9' ]+", " ", t.lower()).split()
# human corpus: TTS of a sample of the human's own messages
import os
if not os.path.exists("human_speech.npy"):
    lines = [l for l in open("human_msgs.txt").read().split("\n") if len(l.split()) >= 6]
    pick = [lines[i] for i in np.random.default_rng(5).choice(len(lines), 260, replace=False)]
    parts = []
    for l in pick:
        x, sr = tts(l); parts += [ff(x, sr, "anull"), np.zeros(8000, dtype=np.float32)]
    np.save("human_speech.npy", np.concatenate(parts)); open("human_speech.txt", "w").write("\n".join(pick))
human = np.load("human_speech.npy")
corp, csr = sf.read("corpus_speech.wav", dtype="float32"); agent = ff(corp, csr, "anull")
htext = norm(open("human_msgs.txt").read()); atext = norm(open("corpus.txt").read())
print(f"human speech {len(human)/16000/60:.1f} min, agent speech {len(agent)/16000/60:.1f} min, human text {len(htext)} words", flush=True)
conds = {"normal": lambda x, sr: ff(x, sr, "anull"), "fast": lambda x, sr: ff(x, sr, "atempo=1.4"), "slow": lambda x, sr: ff(x, sr, "atempo=0.8"), "quiet": lambda x, sr: ff(x, sr, "anull") * 0.15,
         "noisy": lambda x, sr: (lambda y: y + rng.standard_normal(len(y)).astype(np.float32) * 0.03)(ff(x, sr, "anull")), "pitchup": lambda x, sr: ff(x, sr, f"asetrate={int(sr*1.15)},atempo={1/1.15:.4f}"), "pitchdown": lambda x, sr: ff(x, sr, f"asetrate={int(sr*0.87)},atempo={1/0.87:.4f}")}
out = {}
for w in WORDS:
    sp = spotter(w); tgt = norm(w)
    per = {c: [0, 0, 0] for c in conds}   # kws hits, asr exact-end hits, n
    for car in CARRIERS:
        x, sr = tts(f"{car}. {w.capitalize()}.")
        for c, f in conds.items():
            y = np.concatenate([f(x, sr), np.zeros(12000, dtype=np.float32)])
            per[c][0] += run(sp, y) > 0; per[c][1] += norm(asr(y))[-1:] == tgt; per[c][2] += 1
    n = sum(v[2] for v in per.values())
    out[w] = {"kws_hit": sum(v[0] for v in per.values()) / n, "asr_hit": sum(v[1] for v in per.values()) / n, "per_condition_kws": {c: v[0] / v[2] for c, v in per.items()}, "kws_fa_agent_35min": run(sp, agent), "kws_fa_human_min": run(sp, human), "human_text_occ": htext.count(w), "agent_text_occ": atext.count(w)}
    print(w, json.dumps(out[w]), flush=True); json.dump(out, open("endstudy2.json", "w"))
