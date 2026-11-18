"""Does a local denoiser in front of the on-device keyword spotter help? Words (attention katana tomato macaroni) x 4 voices x noise {white, pink, babble} x SNR {0,5,10} dB + clean,
each through: none | afftdn | arnndn(sh) | arnndn(bd) | highpass+arnndn(sh); spotter = the app's (kws.mjs keywords: katana threshold 0.15 boost 2). Writes denoise_study.json.
Babble = other speech (corpus_speech.wav) mixed in; white/pink from ffmpeg-free numpy (pink by 1/f shaping)."""
import json, numpy as np, soundfile as sf, sherpa_onnx
import cancelstudy10 as cs
PH = json.load(open("phrases11.json")); WORDS = ["attention", "katana", "tomato", "macaroni"]; IDX = {w: PH.index(w) for w in WORDS}
VOICES = ["af_heart", "am_adam", "bf_emma", "bm_george"]
RNN = "/tmp/tv_rnn"
MODES = {"none": None, "afftdn": "afftdn=nr=20:nf=-35:tn=1",
         "arnndn_sh": f"aresample=48000,arnndn=m={RNN}/sh.rnnn,aresample=16000", "arnndn_bd": f"aresample=48000,arnndn=m={RNN}/bd.rnnn,aresample=16000",
         "hp_arnndn": f"highpass=f=120,aresample=48000,arnndn=m={RNN}/sh.rnnn,aresample=16000"}
M = cs.M; c = "chunk-16-left-64.int8.onnx"
def make_spotter():
    return sherpa_onnx.KeywordSpotter(tokens=f"{M}/tokens.txt", encoder=f"{M}/encoder-epoch-12-avg-2-{c}", decoder=f"{M}/decoder-epoch-12-avg-2-{c}", joiner=f"{M}/joiner-epoch-12-avg-2-{c}", keywords_file="noisek_kw_katana_only_0.15_b2.txt", keywords_threshold=0.25, keywords_score=1.0, num_threads=1, provider="cpu")
bab, bsr = sf.read("corpus_speech.wav", dtype="float32")
if bab.ndim > 1: bab = bab.mean(1)
def pink(n, rng):
    w = np.fft.rfft(rng.standard_normal(n)); f = np.arange(len(w)); f[0] = 1; return np.fft.irfft(w / np.sqrt(f), n).astype(np.float32)
def noise_for(kind, n, rng, sr):
    if kind == "white": return rng.standard_normal(n).astype(np.float32)
    if kind == "pink": return pink(n, rng)
    k = int(rng.integers(0, len(bab) - n * bsr // sr - 1)); seg = bab[k:k + int(n * bsr / sr)]
    return np.interp(np.linspace(0, len(seg) - 1, n), np.arange(len(seg)), seg).astype(np.float32)
def mix(word16, kind, snr, rng):
    lead, trail = 32000, 24000; n = lead + len(word16) + trail
    if kind == "clean": return np.concatenate([np.zeros(lead, np.float32), word16, np.zeros(trail, np.float32)]) + (rng.standard_normal(n) * 0.002).astype(np.float32)
    nz = noise_for(kind, n, rng, 16000); sp = np.sqrt((word16 ** 2).mean()) + 1e-9; nz = nz / (np.sqrt((nz ** 2).mean()) + 1e-9) * sp / (10 ** (snr / 20))
    return np.concatenate([np.zeros(lead, np.float32), word16, np.zeros(trail, np.float32)]) + nz
def part(mode):
    af = MODES[mode]; sp = make_spotter(); rng = np.random.default_rng(3); res = {}
    for v in VOICES:
        for w in WORDS:
            x, sr = sf.read(f"raw11/{v}__{IDX[w]:02d}.wav", dtype="float32"); b = cs.ff(x, sr, "anull")
            for kind, snr in [("clean", 0)] + [(k, s) for k in ("white", "pink", "babble") for s in (0, 5, 10)]:
                y = mix(b, kind, snr, rng)
                if af: y = cs.ff(y, 16000, af)
                heard = {k.upper() for k in cs.live(sp, y)}
                d = res.setdefault(f"{w}|{kind}|{snr}", [0, 0]); d[0] += w.upper() in heard; d[1] += 1
    return mode, {k: v[0] / v[1] for k, v in res.items()}
if __name__ == "__main__":
    from multiprocessing import Pool
    with Pool(3) as pool: out = dict(pool.map(part, list(MODES)))
    json.dump(out, open("denoise_study.json", "w")); print("DONE", flush=True)
