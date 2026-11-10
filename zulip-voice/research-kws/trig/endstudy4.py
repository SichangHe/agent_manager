"""End-word study 2: candidates vs the kangaroo baseline. Positive: word at the end of a sentence under 7 conditions x 3 carriers (KWS hit and transcript hit).
False accepts: KWS over 35 min of agent speech and over TTS of the human's own Zulip messages; transcript occurrences in 55k words of the human's text."""
import json, re, subprocess, sys, urllib.request
import numpy as np, soundfile as sf, sherpa_onnx
M = "../sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01"
WORDS = "manatee,kimono,okapi,wapiti,kahuna,pinyata,tamale,kalamata,hakuna_matata,katana,mikado,ukulele,mahalo,tapioca,kakapo,tomato,potato,banana,volcano".split(",")
VOICES = ["af_heart", "am_adam", "am_michael", "af_bella", "bf_emma", "bm_george", "af_sarah"]
THRESHOLDS = [0.15, 0.25, 0.35, 0.5]
CARRIERS = ["Please run the build on staging and tell me how it went", "I think we should compare the results with the previous runs", "Yes, go ahead and deploy it to the second machine"]
rng = np.random.default_rng(2)
def ff(x, sr, af):
    p = subprocess.run(["ffmpeg", "-nostdin", "-v", "error", "-f", "f32le", "-ar", str(sr), "-ac", "1", "-i", "pipe:0", "-af", af, "-ar", "16000", "-f", "f32le", "pipe:1"], input=x.astype(np.float32).tobytes(), capture_output=True)
    return np.frombuffer(p.stdout, dtype=np.float32)
def tts(text):
    r = urllib.request.urlopen(urllib.request.Request("http://127.0.0.1:8770/tts", data=json.dumps({"text": text}).encode(), headers={"Content-Type": "application/json"}), timeout=120).read()
    open("/tmp/_t2.wav", "wb").write(r); return sf.read("/tmp/_t2.wav", dtype="float32")
def spotter(word, thr=0.25):
    open(f"e4_{word}_raw.txt", "w").write(word.replace("_", " ").upper() + "\n")
    subprocess.run(["../.venv/bin/sherpa-onnx-cli", "text2token", "--tokens", f"{M}/tokens.txt", "--tokens-type", "bpe", "--bpe-model", f"{M}/bpe.model", f"e4_{word}_raw.txt", f"e4_{word}_tok.txt"], check=True, capture_output=True)
    c = "chunk-16-left-64.int8.onnx"
    return sherpa_onnx.KeywordSpotter(tokens=f"{M}/tokens.txt", encoder=f"{M}/encoder-epoch-12-avg-2-{c}", decoder=f"{M}/decoder-epoch-12-avg-2-{c}", joiner=f"{M}/joiner-epoch-12-avg-2-{c}", keywords_file=f"e4_{word}_tok.txt", keywords_threshold=thr, keywords_score=1.0, num_threads=1, provider="cpu")
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
    for k in range(6):
        try: return asr1(x)
        except Exception: import time; time.sleep(3 * (k + 1))
    return ""
def asr1(x):
    pcm = (np.clip(x, -1, 1) * 32767).astype("<i2").tobytes(); hdr = b"RIFF" + (36 + len(pcm)).to_bytes(4, "little") + b"WAVEfmt " + (16).to_bytes(4, "little") + (1).to_bytes(2, "little") + (1).to_bytes(2, "little") + (16000).to_bytes(4, "little") + (32000).to_bytes(4, "little") + (2).to_bytes(2, "little") + (16).to_bytes(2, "little") + b"data" + len(pcm).to_bytes(4, "little")
    return json.loads(urllib.request.urlopen(urllib.request.Request("http://127.0.0.1:8771/stt", data=hdr + pcm, method="POST"), timeout=300).read())["text"]
norm = lambda t: re.sub(r"[^a-z0-9' ]+", " ", t.lower()).split()

human = np.load("human_speech.npy")
corp, csr = sf.read("corpus_speech.wav", dtype="float32"); agent = ff(corp, csr, "anull")
htext = norm(open("human_msgs.txt").read()); atext = norm(open("corpus.txt").read())
def breath(y):
    n = 6400; env = np.hanning(n).astype(np.float32); nz = rng.standard_normal(n).astype(np.float32)
    b = ff(nz, 16000, "highpass=f=300,lowpass=f=3000") if False else nz
    return np.concatenate([y[:-12000], np.zeros(1600, dtype=np.float32), b[:n] * env * 0.03, np.zeros(8000, dtype=np.float32)])
def conds(x, sr):
    base = ff(x, sr, "anull"); pad = np.zeros(12000, dtype=np.float32)
    return {"normal": np.concatenate([base, pad]), "fast": np.concatenate([ff(x, sr, "atempo=1.4"), pad]), "slow": np.concatenate([ff(x, sr, "atempo=0.8"), pad]), "quiet": np.concatenate([base, pad]) * 0.15,
            "noisy": np.concatenate([base, pad]) + rng.standard_normal(len(base) + 12000).astype(np.float32) * 0.03,
            "pitchup": np.concatenate([ff(x, sr, f"asetrate={int(sr*1.15)},atempo={1/1.15:.4f}"), pad]), "pitchdown": np.concatenate([ff(x, sr, f"asetrate={int(sr*0.87)},atempo={1/0.87:.4f}"), pad]),
            "breath": breath(np.concatenate([base, pad]))}
def study(w):
    sp = spotter(w); tgt = norm(w); per = {}
    for v in VOICES:
        for i in range(3):
            x, sr = sf.read(f"raw3/{v}__{w}__{i}.wav", dtype="float32")
            for c, y in conds(x, sr).items():
                p = per.setdefault(c, [0, 0, 0]); p[0] += run(sp, y) > 0; p[1] += norm(asr(y))[-1:] == tgt; p[2] += 1
    n = sum(v[2] for v in per.values())
    r = {"kws_hit": sum(v[0] for v in per.values()) / n, "asr_hit": sum(v[1] for v in per.values()) / n, "per_condition_kws": {c: v[0] / v[2] for c, v in per.items()}, "kws_fa_agent_35min": run(sp, agent), "kws_fa_human_37min": run(sp, human), "human_text_occ": htext.count(w), "agent_text_occ": atext.count(w)}
    print(w, json.dumps(r), flush=True); return w, r

import os
if not os.path.exists("fa16.npy"):
    parts = [ff(*sf.read(f"raw4/corpus__{v}.wav", dtype="float32")[::-1][::-1], "anull") if False else None for v in []]
    segs = []
    for v in VOICES:
        x, sr = sf.read(f"raw4/corpus__{v}.wav", dtype="float32"); segs.append(ff(x, sr, "anull"))
    segs.append(np.load("human_speech.npy")); c, csr = sf.read("corpus_speech.wav", dtype="float32"); segs.append(ff(c, csr, "anull"))
    np.save("fa16.npy", np.concatenate(segs)); open("fa16.json", "w").write(json.dumps([len(x) for x in segs]))
norm = lambda t: re.sub(r"[^a-z0-9' ]+", " ", t.lower()).split()
def breath(y):
    n = 6400; env = np.hanning(n).astype(np.float32); nz = rng.standard_normal(n).astype(np.float32)
    return np.concatenate([y[:-12000], np.zeros(1600, dtype=np.float32), nz * env * 0.03, np.zeros(8000, dtype=np.float32)])
def conds(x, sr):
    base = ff(x, sr, "anull"); pad = np.zeros(12000, dtype=np.float32); b = np.concatenate([base, pad])
    return {"normal": b, "fast": np.concatenate([ff(x, sr, "atempo=1.4"), pad]), "slow": np.concatenate([ff(x, sr, "atempo=0.8"), pad]), "quiet": b * 0.15,
            "noisy": b + rng.standard_normal(len(b)).astype(np.float32) * 0.03,
            "pitchup": np.concatenate([ff(x, sr, f"asetrate={int(sr*1.15)},atempo={1/1.15:.4f}"), pad]), "pitchdown": np.concatenate([ff(x, sr, f"asetrate={int(sr*0.87)},atempo={1/0.87:.4f}"), pad]), "breath": breath(b)}
def study(w):
    fa = np.load("fa16.npy", mmap_mode="r"); hours = len(fa) / 16000 / 3600; tgt = norm(w.replace("_", " "))
    clips = []; asr_ok = n = 0
    for v in VOICES:
        for i in range(3):
            x, sr = sf.read(f"raw4/{v}__{w}__{i}.wav", dtype="float32")
            for c, y in conds(x, sr).items():
                clips.append((c, y)); n += 1; asr_ok += norm(asr(y))[-len(tgt):] == tgt
    res = {"asr_hit": asr_ok / n, "hours": hours, "by_threshold": {}}
    for thr in THRESHOLDS:
        sp = spotter(w, thr); per = {}
        for c, y in clips: p = per.setdefault(c, [0, 0]); p[0] += run(sp, y) > 0; p[1] += 1
        sp = spotter(w, thr); fa_hits = 0
        for i in range(0, len(fa), 16000 * 600): fa_hits += run(sp, np.asarray(fa[i:i + 16000 * 600]))
        res["by_threshold"][str(thr)] = {"tpr": sum(v[0] for v in per.values()) / n, "per_condition": {c: v[0] / v[1] for c, v in per.items()}, "fa": fa_hits, "fa_per_hour": fa_hits / hours}
        print(w, thr, json.dumps({k: v for k, v in res["by_threshold"][str(thr)].items() if k != "per_condition"}), flush=True)
    return w, res
if __name__ == "__main__":
    from multiprocessing import Pool
    out = {}
    with Pool(len(WORDS)) as pool:
        for w, r in pool.imap_unordered(study, WORDS):
            out[w] = r; json.dump(out, open("endstudy4.json", "w"))
    print("DONE", flush=True)
