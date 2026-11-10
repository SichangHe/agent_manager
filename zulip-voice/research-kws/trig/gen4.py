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
"""Public-domain book prose read aloud by all 7 voices (the false-accept corpus, ~WORDS_PER_VOICE words each) and the candidate words in 3 carriers (raw4/)."""
import os, re, glob, random, sys, soundfile as sf, numpy as np
from kokoro import KPipeline
WORDS = sys.argv[1].split(","); WPV = int(sys.argv[2])
CARRIERS = ["Please run the build on staging and tell me how it went", "I think we should compare the results with the previous runs", "Yes, go ahead and deploy it to the second machine"]
voices = {"af_heart": None, **{v: f"/tmp/kvoices/{v}.pt" for v in ["am_adam", "am_michael", "af_bella", "bf_emma", "bm_george", "af_sarah"]}}
books = []
for f in glob.glob("/tmp/gut/*.txt"):
    t = open(f, errors="ignore").read(); a = t.find("*** START"); b = t.rfind("*** END")
    t = re.sub(r"\s+", " ", t[a + 100:b]); books.append(re.split(r"(?<=[.!?]) ", t))
os.makedirs("raw4", exist_ok=True)
for vi, (v, path) in enumerate(voices.items()):
    pipe = KPipeline(lang_code="b" if v.startswith("b") else "a", repo_id="hexgrad/Kokoro-82M", device="cuda")
    for w in WORDS:
        for i, c in enumerate(CARRIERS):
            f = f"raw4/{v}__{w}__{i}.wav"
            if not os.path.exists(f): sf.write(f, np.concatenate([np.asarray(r.audio) for r in pipe(f"{c}. {w.replace("_", " ").capitalize()}.", voice=path or v)]), 24000)
    out = f"raw4/corpus__{v}.wav"
    if os.path.exists(out): continue
    rng = random.Random(vi); words = 0; parts = []
    while words < WPV:
        s = rng.choice(books); k = rng.randrange(len(s) - 6); text = " ".join(s[k:k + 4])[:500]
        if not re.search(r"[a-z]{3}", text): continue
        try: parts += [np.concatenate([np.asarray(r.audio) for r in pipe(text, voice=path or v)]), np.zeros(9000, dtype=np.float32)]
        except Exception: continue
        words += len(text.split())
    sf.write(out, np.concatenate(parts), 24000); print(v, "corpus", words, "words", flush=True)
