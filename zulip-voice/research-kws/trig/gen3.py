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
"""Every candidate end word in 3 carrier sentences in all 7 voices (GPU 0), 24 kHz wav into `raw3/`."""
import os, soundfile as sf, numpy as np, sys
from kokoro import KPipeline
WORDS = sys.argv[1].split(",")
CARRIERS = ["Please run the build on staging and tell me how it went", "I think we should compare the results with the previous runs", "Yes, go ahead and deploy it to the second machine"]
voices = {"af_heart": None, **{v: f"/tmp/kvoices/{v}.pt" for v in ["am_adam", "am_michael", "af_bella", "bf_emma", "bm_george", "af_sarah"]}}
os.makedirs("raw3", exist_ok=True)
for v, path in voices.items():
    pipe = KPipeline(lang_code="b" if v.startswith("b") else "a", repo_id="hexgrad/Kokoro-82M", device="cuda")
    for w in WORDS:
        for i, c in enumerate(CARRIERS):
            f = f"raw3/{v}__{w}__{i}.wav"
            if os.path.exists(f): continue
            audio = np.concatenate([np.asarray(r.audio) for r in pipe(f"{c}. {w.capitalize()}.", voice=path or v)])
            sf.write(f, audio, 24000)
    print(v, "done", flush=True)
