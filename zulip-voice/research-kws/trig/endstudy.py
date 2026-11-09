import glob, json, os, re, subprocess, sys, time, urllib.request
import numpy as np, soundfile as sf, sherpa_onnx
M = "../sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01"
END = {"over": ("OVER", "Over."), "pineapple": ("PINEAPPLE", "Pineapple."), "avocado": ("AVOCADO", "Avocado."), "kangaroo": ("KANGAROO", "Kangaroo."), "volcano": ("VOLCANO", "Volcano."), "watermelon": ("WATERMELON", "Watermelon."), "tornado": ("TORNADO", "Tornado."), "pistachio": ("PISTACHIO", "Pistachio."), "bazooka": ("BAZOOKA", "Bazooka."), "banana": ("BANANA", "Banana."), "buffalo": ("BUFFALO", "Buffalo.")}
CARRIERS = ["Please run the build on staging and tell me how it went", "I think we should compare the results with the previous runs", "Yes, go ahead and deploy it to the second machine"]
rng = np.random.default_rng(2)
def ff(x, sr, af):
    p = subprocess.run(["ffmpeg", "-nostdin", "-v", "error", "-f", "f32le", "-ar", str(sr), "-ac", "1", "-i", "pipe:0", "-af", af, "-ar", "16000", "-f", "f32le", "pipe:1"], input=x.astype(np.float32).tobytes(), capture_output=True)
    return np.frombuffer(p.stdout, dtype=np.float32)
def tts(text):
    r = urllib.request.urlopen(urllib.request.Request("http://127.0.0.1:8770/tts", data=json.dumps({"text": text}).encode(), headers={"Content-Type": "application/json"}), timeout=60).read()
    open("/tmp/_t.wav", "wb").write(r); x, sr = sf.read("/tmp/_t.wav", dtype="float32"); return x, sr
def spotter(raw):
    open("e_raw.txt", "w").write(raw + "\n")
    subprocess.run(["../.venv/bin/sherpa-onnx-cli", "text2token", "--tokens", f"{M}/tokens.txt", "--tokens-type", "bpe", "--bpe-model", f"{M}/bpe.model", "e_raw.txt", "e_tok.txt"], check=True, capture_output=True)
    return sherpa_onnx.KeywordSpotter(tokens=f"{M}/tokens.txt", encoder=f"{M}/encoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx", decoder=f"{M}/decoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx", joiner=f"{M}/joiner-epoch-12-avg-2-chunk-16-left-64.int8.onnx", keywords_file="e_tok.txt", keywords_threshold=0.25, keywords_score=1.0, num_threads=1, provider="cpu")
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
    return json.loads(urllib.request.urlopen(urllib.request.Request("http://127.0.0.1:8771/stt", data=hdr + pcm, method="POST"), timeout=60).read())["text"]
norm = lambda t: re.sub(r"[^a-z0-9' ]+", " ", t.lower()).split()
# false-accept corpus: agent text as ordinary speech, ends of sentences followed by silence (every sentence end is a chance)
corp, csr = sf.read("corpus_speech.wav", dtype="float32"); corp16 = ff(corp, csr, "anull")
out = {}
for k, (kw, text) in END.items():
    sp = spotter(kw); target = norm(text)
    hit_k = hit_a = n = 0
    for c in CARRIERS:
        for voice_text in (f"{c}. {text}",):
            x, sr = tts(voice_text); clip = np.concatenate([ff(x, sr, "anull"), np.zeros(12000, dtype=np.float32)])
            for name, y in {"normal": clip, "fast": np.concatenate([ff(x, sr, "atempo=1.4"), np.zeros(12000, dtype=np.float32)]), "quiet": clip * 0.15, "noisy": clip + rng.standard_normal(len(clip)).astype(np.float32) * 0.03}.items():
                n += 1; hit_k += run(sp, y) > 0
                w = norm(asr(y)); hit_a += (w[-len(target):] == target) or (w[-len(target) - 0:][:len(target)] == target)
    fa = run(sp, corp16)
    # ASR-side false accepts: sentences in the corpus whose last words equal the phrase
    words = norm(open("corpus.txt").read()); t = target; asr_fa = sum(1 for i in range(len(words) - len(t) + 1) if words[i:i + len(t)] == t)
    out[k] = {"kws_hit": hit_k / n, "asr_hit": hit_a / n, "kws_fa_35min": fa, "text_occurrences": asr_fa}
    print(k, out[k], flush=True)
    json.dump(out, open("endstudy.json", "w"))
