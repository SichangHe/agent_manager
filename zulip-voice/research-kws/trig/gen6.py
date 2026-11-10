# /// script
# requires-python = ">=3.12"
# dependencies = ["kokoro==0.9.4", "soundfile>=0.13,<0.14", "torch==2.9.1+cu128"]
# [[tool.uv.index]]
# name = "pytorch-cuda"
# url = "https://download.pytorch.org/whl/cu128"
# explicit = true
# [tool.uv.sources]
# torch = { index = "pytorch-cuda" }
# ///
"""Each phrase of PHRASES alone, in all 7 voices (raw6/)."""
import sys, json
import os, soundfile as sf, numpy as np
from kokoro import KPipeline
PHRASES = json.load(open("phrases6.json"))
voices = {"af_heart": None, **{v: f"/tmp/kvoices/{v}.pt" for v in ["am_adam", "am_michael", "af_bella", "bf_emma", "bm_george", "af_sarah"]}}
os.makedirs("raw6", exist_ok=True)
for v, path in voices.items():
    pipe = KPipeline(lang_code="b" if v.startswith("b") else "a", repo_id="hexgrad/Kokoro-82M", device="cuda")
    for i, ph in enumerate(PHRASES):
        f = f"raw6/{v}__{i:02d}.wav"
        if not os.path.exists(f): sf.write(f, np.concatenate([np.asarray(r.audio) for r in pipe(ph.capitalize() + ".", voice=path or v)]), 24000)
    print(v, "done", flush=True)
