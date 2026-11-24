"""With the app's words loaded together (attention, tomato, potato = command, tornado = cancel), which of them fire on every word's clips: cross-triggers."""
import json, sys, numpy as np, soundfile as sf
import cancelstudy10 as cs, study14 as s14
APP = ["attention", "tomato", "potato", "tornado"]
PH = json.load(open("phrases14.json"))
sp = cs.spotter(APP, 0.25, 0.0, "conf14")
out = {}
for i, w in enumerate(PH):
    hits = {}
    for v in cs.VOICES:
        x, sr = sf.read(f"raw14/{v}__{i:02d}.wav", dtype="float32")
        for c, y in s14.conds(x, sr).items():
            for k in set(h.replace("_", " ").lower() for h in cs.live(sp, y)):
                hits[k] = hits.get(k, 0) + 1
    out[w] = hits; print(w, hits, flush=True)
json.dump(out, open("confuse14.json", "w"))
