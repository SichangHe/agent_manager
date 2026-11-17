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
"""CPU Kokoro: every trigger phrase in every available voice (the GPUs are busy), 24 kHz wav into `raw/`."""
import os, sys, soundfile as sf, numpy as np
from kokoro import KPipeline
phrases = {"pause": "Pause.", "heymira": "Hey Mira.", "oknova": "Okay Nova.", "heyzulip": "Hey Zulip.", "holdit": "Hold it.", "attention": "Attention.", "stopreading": "Stop reading.", "mayday": "Mayday.", "heyheyhey": "Hey, hey, hey.", "five": "Five."}
voices = {"af_heart": None, **{v: f"/tmp/kvoices/{v}.pt" for v in ["am_adam", "am_michael", "af_bella", "bf_emma", "bm_george", "af_sarah"]}}
os.makedirs("raw", exist_ok=True)
for v, path in voices.items():
    lang = "b" if v.startswith("b") else "a"
    pipe = KPipeline(lang_code=lang, repo_id="hexgrad/Kokoro-82M", device="cpu")
    for k, text in phrases.items():
        if os.path.exists(f"raw/{v}__{k}.wav"): continue
        audio = np.concatenate([np.asarray(r.audio) for r in pipe(text, voice=path or v)])
        sf.write(f"raw/{v}__{k}.wav", audio, 24000)
    print(v, "done", flush=True)
