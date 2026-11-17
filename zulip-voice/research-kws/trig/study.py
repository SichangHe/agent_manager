import glob, json, os, re, subprocess, sys, time, urllib.request
import numpy as np, soundfile as sf, sherpa_onnx
M = "../sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01"
PHR = {"heyheyhey": "HEY HEY HEY", "five": "FIVE", "pause": "PAUSE", "heymira": "HEY MIRA", "oknova": "OKAY NOVA", "heyzulip": "HEY ZULIP", "holdit": "HOLD IT", "attention": "ATTENTION", "stopreading": "STOP READING", "mayday": "MAYDAY"}
SAY = {"heyheyhey": "hey hey hey", "five": "five", "pause": "pause", "heymira": "hey mira", "oknova": "okay nova", "heyzulip": "hey zulip", "holdit": "hold it", "attention": "attention", "stopreading": "stop reading", "mayday": "mayday"}
rng = np.random.default_rng(1)
def ff(x, sr, af):
    p = subprocess.run(["ffmpeg", "-nostdin", "-v", "error", "-f", "f32le", "-ar", str(sr), "-ac", "1", "-i", "pipe:0", "-af", af, "-ar", "16000", "-f", "f32le", "pipe:1"], input=x.astype(np.float32).tobytes(), capture_output=True)
    return np.frombuffer(p.stdout, dtype=np.float32)
def noise(n, color="pink"):
    w = rng.standard_normal(n).astype(np.float32); f = np.fft.rfft(w); k = np.arange(len(f)); k[0] = 1
    f = f / (k ** (0.5 if color == "pink" else 1.0)); x = np.fft.irfft(f, n).astype(np.float32); return x / np.abs(x).max()
speech, ssr = sf.read("corpus_speech.wav", dtype="float32"); speech16 = ff(speech, ssr, "anull")
def variants(x24):
    base = ff(x24, 24000, "anull"); out = {"normal": base}
    out["fast"] = ff(x24, 24000, "atempo=1.4"); out["quiet"] = base * 0.12
    out["room"] = ff(x24, 24000, "volume=0.3,highpass=f=300,lowpass=f=3000,aecho=0.8:0.6:40:0.4")
    # double talk under playback: echo residual of agent speech at 0.12, the user's word attenuated by the suppressor (0.5) and pink noise at -25 dB
    n = 16000 * 6; off = rng.integers(0, len(speech16) - n); bed = speech16[off:off + n] * 0.12 + noise(n) * 0.03
    pos = rng.integers(8000, n - len(base) - 8000); dt = bed.copy(); dt[pos:pos + len(base)] += base * 0.5; out["doubletalk"] = dt
    n2 = len(base) + 16000; nz = noise(n2) * 0.1; nz[8000:8000 + len(base)] += base * 0.4; out["noisy"] = nz
    return out
def spotter(kw):
    open("kw_one.txt", "w").write(kw)
    return sherpa_onnx.KeywordSpotter(tokens=f"{M}/tokens.txt", encoder=f"{M}/encoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx", decoder=f"{M}/decoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx", joiner=f"{M}/joiner-epoch-12-avg-2-chunk-16-left-64.int8.onnx", keywords_file="kw_one.txt", keywords_threshold=0.25, keywords_score=1.0, num_threads=1, provider="cpu")
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
def whisper(x):
    pcm = (np.clip(x, -1, 1) * 32767).astype("<i2").tobytes(); hdr = b"RIFF" + (36 + len(pcm)).to_bytes(4, "little") + b"WAVEfmt " + (16).to_bytes(4, "little") + (1).to_bytes(2, "little") + (1).to_bytes(2, "little") + (16000).to_bytes(4, "little") + (32000).to_bytes(4, "little") + (2).to_bytes(2, "little") + (16).to_bytes(2, "little") + b"data" + len(pcm).to_bytes(4, "little")
    for attempt in range(60):
        try:
            r = urllib.request.urlopen(urllib.request.Request("http://127.0.0.1:8771/stt", data=hdr + pcm, method="POST"), timeout=60); return json.loads(r.read())["text"]
        except Exception: time.sleep(5)
    return ""
norm = lambda t: re.sub(r"[^a-z0-9 ]+", " ", t.lower()).split()
res = {k: {} for k in PHR}; wh = {k: {} for k in PHR}
vfiles = sorted(glob.glob("raw/*.wav"))
sp_by = {k: None for k in PHR}
for k, kw in PHR.items():
    open("kw_raw1.txt", "w").write(kw + "\n")
    subprocess.run(["../.venv/bin/sherpa-onnx-cli", "text2token", "--tokens", f"{M}/tokens.txt", "--tokens-type", "bpe", "--bpe-model", f"{M}/bpe.model", "kw_raw1.txt", "kw_tok.txt"], check=True, capture_output=True)
    sp = spotter(open("kw_tok.txt").read())
    for f in sorted(glob.glob('raw/*.wav')):
        v, ph = os.path.basename(f)[:-4].split("__")
        if ph != k: continue
        x, sr = sf.read(f, dtype="float32")
        for name, clip in variants(x).items():
            res[k].setdefault(name, []).append(run(sp, clip) > 0)
            w = norm(whisper(clip)); target = SAY[k].split()
            wh[k].setdefault(name, []).append(any(w[i:i + len(target)] == target for i in range(len(w))))
    # false accepts: 35 min of agent speech + 5 min of mixed noise
    t0 = time.time(); fa_speech = run(sp, speech16); noisy = np.concatenate([noise(16000 * 60, c) * a for c in ("white", "pink", "brown") for a in (0.1, 0.4)]); fa_noise = run(sp, noisy)
    res[k]["_fa_speech"] = fa_speech; res[k]["_fa_noise"] = fa_noise; print(k, "fa", fa_speech, fa_noise, f"{time.time()-t0:.0f}s", flush=True)
json.dump({"kws": res, "whisper": wh}, open("study.json", "w"))
