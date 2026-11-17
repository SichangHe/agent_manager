# /// script
# requires-python = ">=3.12"
# dependencies = ["kokoro==0.9.4", "soundfile>=0.13,<0.14", "torch==2.9.1+cu128", "sherpa-onnx==2.0.1", "numpy"]
# [[tool.uv.index]]
# name = "pytorch-cuda"
# url = "https://download.pytorch.org/whl/cu128"
# explicit = true
# [tool.uv.sources]
# torch = { index = "pytorch-cuda" }
# ///
import glob, os, subprocess, json, urllib.request, numpy as np, soundfile as sf, sherpa_onnx
M="../sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01"
toks=[l.strip() for l in open('../kw_ans.txt')]; words=[l.strip() for l in open('../kw_ans_raw.txt')]
kw="\n".join(f"{t} @{w}" for t,w in zip(toks,words)); open('kw_ans_tagged.txt','w').write(kw+"\n")
sp=sherpa_onnx.KeywordSpotter(tokens=f"{M}/tokens.txt",encoder=f"{M}/encoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx",decoder=f"{M}/decoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx",joiner=f"{M}/joiner-epoch-12-avg-2-chunk-16-left-64.int8.onnx",keywords_file="kw_ans_tagged.txt",keywords_threshold=0.25,keywords_score=1.0,num_threads=1,provider="cpu")
def run(x):
    s=sp.create_stream(); hits=[]
    for i in range(0,len(x),1600):
        s.accept_waveform(16000,x[i:i+1600])
        while sp.is_ready(s):
            sp.decode_stream(s); r=sp.get_result(s)
            if r: hits.append(r); sp.reset_stream(s)
    s.accept_waveform(16000,np.zeros(8000,dtype=np.float32)); s.input_finished()
    while sp.is_ready(s):
        sp.decode_stream(s); r=sp.get_result(s)
        if r: hits.append(r); sp.reset_stream(s)
    return hits
def to16(x,sr): return np.frombuffer(subprocess.run(["ffmpeg","-nostdin","-v","error","-f","f32le","-ar",str(sr),"-ac","1","-i","pipe:0","-ar","16000","-f","f32le","pipe:1"],input=x.astype(np.float32).tobytes(),capture_output=True).stdout,dtype=np.float32)
# detection: each word, 7 voices is unavailable on the worker (one voice); use Kokoro CPU raw voices if present
import glob
from kokoro import KPipeline
voices={"af_heart":None,"am_adam":"/tmp/kvoices/am_adam.pt","am_michael":"/tmp/kvoices/am_michael.pt","af_bella":"/tmp/kvoices/af_bella.pt","bf_emma":"/tmp/kvoices/bf_emma.pt","bm_george":"/tmp/kvoices/bm_george.pt","af_sarah":"/tmp/kvoices/af_sarah.pt"}
phr={"YES":"Yes.","NO":"No.","ONE":"One.","TWO":"Two.","THREE":"Three.","CANCEL":"Cancel."}
det={w:0 for w in phr}; n=0
for v,path in voices.items():
    pipe=KPipeline(lang_code="b" if v.startswith("b") else "a",repo_id="hexgrad/Kokoro-82M",device="cpu"); n+=1
    for w,t in phr.items():
        a=np.concatenate([np.asarray(r.audio) for r in pipe(t,voice=path or v)]); hits=run(to16(a,24000))
        det[w]+= any(h==w for h in hits)
print("detection (7 voices):",{w:f"{d}/{n}" for w,d in det.items()})
x,sr=sf.read("corpus_speech.wav",dtype="float32"); hits=run(to16(x,sr)); mins=len(x)/sr/60
from collections import Counter; c=Counter(hits)
print(f"false hits in {mins:.0f} min of agent speech:",{w:c.get(w,0) for w in phr}, "per 20 s window:",{w:round(c.get(w,0)/mins/3,3) for w in phr})
