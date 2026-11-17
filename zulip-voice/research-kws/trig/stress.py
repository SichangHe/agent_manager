"""Voice-control stress test: every start/command/cancel/end word and command, 7 voices x 7 conditions.
(a) on-device keyword spotter with the app's own keyword pieces and threshold (kws.mjs): hit rate per word and condition
(b) speech to text through the production STT worker (loopback): transcripts, classified later by classify.mjs with the app's grammar
Writes stress.json."""
import sys, json, io, urllib.request, numpy as np, soundfile as sf, sherpa_onnx
import cancelstudy10 as cs
PH = json.load(open("phrases11.json")); VOICES = cs.VOICES
KW = {"ATTENTION": "▁AT T ENT ION", "TOMATO": "▁TO MA T O", "KATANA": "▁K AT AN A", "MACARONI": "▁MA C AR ON I"}   # as in v2/kws.mjs
open("stress_kw.txt", "w").write("\n".join(f"{p} @{t}" for t, p in KW.items()) + "\n")
M = cs.M; c = "chunk-16-left-64.int8.onnx"
sp = sherpa_onnx.KeywordSpotter(tokens=f"{M}/tokens.txt", encoder=f"{M}/encoder-epoch-12-avg-2-{c}", decoder=f"{M}/decoder-epoch-12-avg-2-{c}", joiner=f"{M}/joiner-epoch-12-avg-2-{c}", keywords_file="stress_kw.txt", keywords_threshold=0.25, keywords_score=1.0, num_threads=1, provider="cpu")
def live(y):
    sp_ = sp
    return cs.live(sp_, y)
def stt(y):
    b = io.BytesIO(); sf.write(b, y, 16000, format="WAV", subtype="PCM_16")
    r = urllib.request.Request("http://127.0.0.1:8771/stt", data=b.getvalue(), method="POST")
    try: return json.loads(urllib.request.urlopen(r, timeout=60).read()).get("text", "")
    except Exception as e: return "!ERR " + str(e)[:60]
out = []
for v in VOICES:
    for i, ph in enumerate(PH):
        x, sr = sf.read(f"raw11/{v}__{i:02d}.wav", dtype="float32")
        for cond, y in cs.conds(x, sr).items():
            heard = sorted({k.replace("_", " ").upper() for k in live(y)}) if True else []
            out.append({"voice": v, "phrase": ph, "cond": cond, "kws": heard, "stt": stt(y)})
    print(v, "done", flush=True)
json.dump(out, open("stress.json", "w"))
print("DONE", flush=True)
