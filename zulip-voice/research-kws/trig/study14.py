"""No-K start/cancel word study (🧑 "In noise background, the K sound don't register at all ... T sound is good, and vowels are good"); babble = 3 overlapping speakers from corpus_speech.wav at SNR 10/5/0 dB. Was: command-word candidate study: true-positive rate of every word of phrases14.json, 7 voices x conditions (clean, noise 0.01/0.03/0.06, fast, slow, pitch up/down, quiet).
Spotter settings: A = default (threshold 0.25), B = sensitive (0.15, boost 2). Usage: python cmd12_study.py (writes study14.json)"""
import json, numpy as np, soundfile as sf
import cancelstudy10 as cs
PH = json.load(open("phrases14.json")); VOICES = cs.VOICES
CONFIGS = {"A_0.25": (0.25, 0.0), "B_0.15_b2": (0.15, 2.0)}
rng = np.random.default_rng(14)
import subprocess
NOISE = '/home/sichangheagent/noise_corpora'
def load16(path, max_s=60):
    return np.frombuffer(subprocess.run(['ffmpeg','-nostdin','-v','error','-i',path,'-t',str(max_s),'-ar','16000','-ac','1','-f','f32le','pipe:1'],capture_output=True,check=True).stdout, dtype=np.float32)
import glob, csv, random
_r = random.Random(13)
# 🧑 "use mixed audio with water sound and garbled music and a market and so on, instead of white noise, which is trivial to beat"
WATER = np.concatenate([load16(f'{NOISE}/esc50_dl/ESC-50-master/audio/{row["filename"]}') for row in csv.DictReader(open(f'{NOISE}/esc50_dl/ESC-50-master/meta/esc50.csv')) if row['category'] in ('pouring_water','water_drops','rain','sea_waves','toilet_flush')])
MUSIC = np.concatenate([load16(f, 40) for f in _r.sample(sorted(glob.glob(f'{NOISE}/musan_dl/musan/music/*/*.wav')), 40)])
_bg = [l.strip() for l in open(f'{NOISE}/musan_dl/musan/noise/free-sound/ANNOTATIONS').read().split('Background noises:')[1].split('\n') if l.strip().startswith('noise-')]
CROWD = np.concatenate([load16(f'{NOISE}/musan_dl/musan/noise/free-sound/{n}.wav', 60) for n in _bg])
BAB = np.frombuffer(subprocess.run(['ffmpeg','-nostdin','-v','error','-i','corpus_speech.wav','-ar','16000','-ac','1','-f','f32le','pipe:1'],capture_output=True,check=True).stdout, dtype=np.float32)
def conds(x, sr):
    b = cs.ff(x, sr, "anull")
    lead = lambda: (rng.standard_normal(32000) * 0.002).astype(np.float32); trail = lambda: (rng.standard_normal(24000) * 0.002).astype(np.float32)
    mk = lambda w: np.concatenate([lead(), w, trail()])
    out = {"clean": mk(b), "quiet": mk(b * 0.1), "fast": mk(cs.ff(x, sr, "atempo=1.4")), "slow": mk(cs.ff(x, sr, "atempo=0.8")),
           "pitchup": mk(cs.ff(x, sr, "asetrate=%d,atempo=%.4f" % (int(sr * 1.15), 1 / 1.15))), "pitchdown": mk(cs.ff(x, sr, "asetrate=%d,atempo=%.4f" % (int(sr * 0.87), 1 / 0.87)))}
    def seg(src, n):
        s0 = rng.integers(0, len(src) - n); x = src[s0:s0 + n].astype(np.float32); return x / (np.sqrt(np.mean(x ** 2)) + 1e-9)
    def add(y, noise, snr):
        return y + noise * np.sqrt(np.mean(b ** 2) / 10 ** (snr / 10))
    for snr in (10, 5, 0):
        y = mk(b); n = len(y)
        babble = (lambda z: z / (np.sqrt(np.mean(z ** 2)) + 1e-9))(seg(BAB, n) + seg(BAB, n) + seg(BAB, n))
        mix = (lambda z: z / (np.sqrt(np.mean(z ** 2)) + 1e-9))(seg(WATER, n) + seg(MUSIC, n) + seg(CROWD, n) + babble)
        out[f"mix{snr}dB"] = add(y, mix, snr)   # water + music + street/crowd + 3-speaker chatter together
    for name, src in (("water", WATER), ("music", MUSIC), ("crowd", CROWD)):
        y = mk(b); out[f"{name}5dB"] = add(y, seg(src, len(y)), 5)
    y = mk(b); out["babble5dB"] = add(y, (lambda z: z / (np.sqrt(np.mean(z ** 2)) + 1e-9))(seg(BAB, len(y)) + seg(BAB, len(y)) + seg(BAB, len(y))), 5)
    return out
def part(ci):
    thr, boost = list(CONFIGS.values())[ci]; sp = cs.spotter(PH, thr, boost, f"w14_{ci}")
    tp = {p: {} for p in PH}
    for v in VOICES:
        for i, ph in enumerate(PH):
            x, sr = sf.read(f"raw14/{v}__{i:02d}.wav", dtype="float32")
            for c, y in conds(x, sr).items():
                h = [k.replace("_", " ").lower() for k in cs.live(sp, y)]
                d = tp[ph].setdefault(c, [0, 0]); d[0] += ph in h; d[1] += 1
    fa = np.load("fa16.npy", mmap_mode="r"); counts = {}
    for i in range(0, len(fa), 16000 * 600):
        for k in cs.live(sp, np.asarray(fa[i:i + 16000 * 600])): kk = k.replace("_", " ").lower(); counts[kk] = counts.get(kk, 0) + 1
    return list(CONFIGS)[ci], {"tp": {p: {c: v[0] / v[1] for c, v in d.items()} for p, d in tp.items()}, "fa": counts, "hours": len(fa) / 16000 / 3600}
if __name__ == "__main__":
    from multiprocessing import Pool
    with Pool(2) as pool: out = dict(pool.map(part, range(2)))
    json.dump(out, open("study14.json", "w")); print("DONE", flush=True)
