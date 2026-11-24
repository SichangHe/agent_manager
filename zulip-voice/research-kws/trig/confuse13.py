"""Which keyword fires on each candidate's clips when the app's words are loaded together (attention, tomato, and the candidates): cross-triggers."""
import json, numpy as np, soundfile as sf
import cancelstudy10 as cs, study13 as s13
WORDS = ["attention", "tomato", "tornado", "toronto", "tuesday", "potato", "katana", "cancel"]
PH = json.load(open("phrases13.json"))
sp = cs.spotter(WORDS, 0.25, 0.0, "conf13")
out = {}
for w in ["tornado", "toronto", "tuesday", "potato", "tomato", "katana", "cancel"]:
    i = PH.index(w); hits = {}
    for v in cs.VOICES:
        x, sr = sf.read(f"raw13/{v}__{i:02d}.wav", dtype="float32")
        for c, y in s13.conds(x, sr).items():
            for k in set(h.replace("_", " ").lower() for h in cs.live(sp, y)):
                hits[k] = hits.get(k, 0) + 1
    out[w] = hits; print(w, hits, flush=True)
json.dump(out, open("confuse13.json", "w"))
