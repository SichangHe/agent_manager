"""Noisy-room study for the start/command/end/cancel words: hit rate per noise level for several spotter settings (threshold, boost).
Words said alone after a pause (as the app hears them), 7 voices x noise std {0, 0.01, 0.03, 0.06}. Writes noise_study_katana.json."""
import json, subprocess, numpy as np, soundfile as sf, sherpa_onnx
import cancelstudy10 as cs
PH = json.load(open("phrases11.json"))
WORDS = {"attention": "▁AT T ENT ION", "katana": "▁K AT AN A", "tomato": "▁TO MA T O", "macaroni": "▁MA C AR ON I"}
IDX = {w: PH.index(w) for w in WORDS}
CONFIGS = {"now": (0.25, 0.0), "katana_only_0.15_b2": (0.25, 0.0)}
NOISE = [0.0, 0.01, 0.03, 0.06]
M = cs.M; c = "chunk-16-left-64.int8.onnx"; rng = np.random.default_rng(11)
def spotter(thr, boost, tag):
    lines = [f"{p} {(':%s ' % boost) if boost else ''}{'#0.15 :2.0 ' if (tag.startswith('katana_only') and w == 'katana') else ''}@{w.upper()}" for w, p in WORDS.items()]
    open(f"noisek_kw_{tag}.txt", "w").write("\n".join(lines) + "\n")
    return sherpa_onnx.KeywordSpotter(tokens=f"{M}/tokens.txt", encoder=f"{M}/encoder-epoch-12-avg-2-{c}", decoder=f"{M}/decoder-epoch-12-avg-2-{c}", joiner=f"{M}/joiner-epoch-12-avg-2-{c}", keywords_file=f"noisek_kw_{tag}.txt", keywords_threshold=thr, keywords_score=1.0, num_threads=1, provider="cpu")
clips = {}
for v in cs.VOICES:
    for w, i in IDX.items():
        x, sr = sf.read(f"raw11/{v}__{i:02d}.wav", dtype="float32"); b = cs.ff(x, sr, "anull")
        clips[(v, w)] = b
out = {}
for name, (thr, boost) in CONFIGS.items():
    sp = spotter(thr, boost, name); res = {}
    for w in WORDS:
        for n in NOISE:
            hit = 0
            for v in cs.VOICES:
                b = clips[(v, w)]
                y = np.concatenate([(rng.standard_normal(32000)*0.002).astype(np.float32), b, (rng.standard_normal(24000)*0.002).astype(np.float32)])
                if n: y = y + (rng.standard_normal(len(y)) * n).astype(np.float32)
                hit += w.upper() in {k.upper() for k in cs.live(sp, y)}
            res[f"{w}@{n}"] = hit / len(cs.VOICES)
    out[name] = res; print(name, res, flush=True)
json.dump(out, open("noise_study_katana.json", "w")); print("DONE", flush=True)
