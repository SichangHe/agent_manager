"""False accepts of the app's four voice-control words over 4.9 h of speech (fa16.npy), same spotter settings as the app."""
import json, numpy as np, sherpa_onnx
import cancelstudy10 as cs
M = cs.M; c = "chunk-16-left-64.int8.onnx"
sp = sherpa_onnx.KeywordSpotter(tokens=f"{M}/tokens.txt", encoder=f"{M}/encoder-epoch-12-avg-2-{c}", decoder=f"{M}/decoder-epoch-12-avg-2-{c}", joiner=f"{M}/joiner-epoch-12-avg-2-{c}", keywords_file="stress_kw.txt", keywords_threshold=0.25, keywords_score=1.0, num_threads=1, provider="cpu")
fa = np.load("fa16.npy", mmap_mode="r"); hours = len(fa) / 16000 / 3600; counts = {}
for i in range(0, len(fa), 16000 * 600):
    for k in cs.live(sp, np.asarray(fa[i:i + 16000 * 600])): counts[k] = counts.get(k, 0) + 1
    print(i // 16000 // 60, "min", counts, flush=True)
json.dump({"hours": hours, "false_accepts": counts}, open("stress_fa.json", "w")); print("DONE", flush=True)
