"""Command-phrase study: one multi-keyword spotter with every phrase; per-phrase hit rate (7 voices x 7 conditions) and false accepts on 4.9 h of ordinary speech, at several thresholds."""
import json, re, subprocess, sys, time
import numpy as np, soundfile as sf, sherpa_onnx
sys.path.insert(0, ".")
M = "../sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01"
import json
PHRASES_ALL = json.load(open("phrases6.json")); SETS = json.load(open("sets6.json"))
PHRASES = PHRASES_ALL
VOICES = ["af_heart", "am_adam", "am_michael", "af_bella", "bf_emma", "bm_george", "af_sarah"]
THRESHOLDS = [0.15, 0.3, 0.5]
GROUP = "A"
rng = np.random.default_rng(3)
def ff(x, sr, af):
    p = subprocess.run(["ffmpeg", "-nostdin", "-v", "error", "-f", "f32le", "-ar", str(sr), "-ac", "1", "-i", "pipe:0", "-af", af, "-ar", "16000", "-f", "f32le", "pipe:1"], input=x.astype(np.float32).tobytes(), capture_output=True)
    return np.frombuffer(p.stdout, dtype=np.float32)
def spotter(thr):
    open(f"c6_{GROUP}_raw.txt", "w").write("\n".join(p.upper() for p in PHRASES) + "\n")
    subprocess.run(["../.venv/bin/sherpa-onnx-cli", "text2token", "--tokens", f"{M}/tokens.txt", "--tokens-type", "bpe", "--bpe-model", f"{M}/bpe.model", f"c6_{GROUP}_raw.txt", f"c6_{GROUP}_tok.txt"], check=True, capture_output=True)
    lines = [f"{t.strip()} @{p.upper().replace(' ', '_')}" for t, p in zip(open(f"c6_{GROUP}_tok.txt").read().strip().split("\n"), PHRASES)]
    open(f"c6_{GROUP}_tagged.txt", "w").write("\n".join(lines) + "\n")
    c = "chunk-16-left-64.int8.onnx"
    return sherpa_onnx.KeywordSpotter(tokens=f"{M}/tokens.txt", encoder=f"{M}/encoder-epoch-12-avg-2-{c}", decoder=f"{M}/decoder-epoch-12-avg-2-{c}", joiner=f"{M}/joiner-epoch-12-avg-2-{c}", keywords_file=f"c6_{GROUP}_tagged.txt", keywords_threshold=thr, keywords_score=1.0, num_threads=1, provider="cpu")
def run(sp, x):
    s = sp.create_stream(); hits = []
    for i in range(0, len(x), 1600):
        s.accept_waveform(16000, x[i:i + 1600])
        while sp.is_ready(s):
            sp.decode_stream(s)
            r = sp.get_result(s)
            if r: hits.append(r); sp.reset_stream(s)
    s.accept_waveform(16000, np.zeros(8000, dtype=np.float32)); s.input_finished()
    while sp.is_ready(s):
        sp.decode_stream(s)
        r = sp.get_result(s)
        if r: hits.append(r); sp.reset_stream(s)
    return hits
def conds(x, sr):
    b = np.concatenate([ff(x, sr, "anull"), np.zeros(8000, dtype=np.float32)])
    return {"normal": b, "fast": np.concatenate([ff(x, sr, "atempo=1.4"), np.zeros(8000, dtype=np.float32)]), "slow": np.concatenate([ff(x, sr, "atempo=0.8"), np.zeros(8000, dtype=np.float32)]), "quiet": b * 0.15,
            "noisy": b + rng.standard_normal(len(b)).astype(np.float32) * 0.03, "pitchup": np.concatenate([ff(x, sr, "asetrate=%d,atempo=%.4f" % (int(sr*1.15), 1/1.15)), np.zeros(8000, dtype=np.float32)]), "pitchdown": np.concatenate([ff(x, sr, "asetrate=%d,atempo=%.4f" % (int(sr*0.87), 1/0.87)), np.zeros(8000, dtype=np.float32)])}
def part(job):
    global PHRASES, GROUP
    GROUP, thr_i = job; PHRASES = SETS[GROUP]; thr = THRESHOLDS[thr_i]; sp = spotter(thr)
    tp = {p: [0, 0, 0] for p in PHRASES}   # right label, wrong label, n
    for v in VOICES:
        for i, ph in enumerate(PHRASES):
            x, sr = sf.read(f"raw6/{v}__{PHRASES_ALL.index(ph):02d}.wav", dtype="float32")
            for c, y in conds(x, sr).items():
                h = [k.replace("_", " ").lower() for k in run(sp, y)]
                tp[ph][2] += 1; tp[ph][0] += ph in h; tp[ph][1] += any(k != ph for k in h)
    fa = np.load("fa16.npy", mmap_mode="r"); hours = len(fa) / 16000 / 3600; counts = {}; sp = spotter(thr)
    t0 = time.time()
    for i in range(0, len(fa), 16000 * 600):
        for k in run(sp, np.asarray(fa[i:i + 16000 * 600])): counts[k.replace("_", " ").lower()] = counts.get(k.replace("_", " ").lower(), 0) + 1
    return f"{GROUP}@{thr}", {"tp": {p: {"hit": v[0] / v[2], "wrong": v[1] / v[2]} for p, v in tp.items()}, "fa_per_hour": {k: n / hours for k, n in counts.items()}, "fa_total_per_hour": sum(counts.values()) / hours, "hours": hours, "fa_sec": time.time() - t0}
if __name__ == "__main__":
    from multiprocessing import Pool
    with Pool(6) as pool: out = dict(pool.map(part, [(g, i) for g in "AB" for i in range(3)]))
    json.dump(out, open("cmdstudy2.json", "w")); print("DONE", flush=True)
