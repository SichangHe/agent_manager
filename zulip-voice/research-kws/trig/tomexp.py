import sys, json, subprocess, numpy as np, soundfile as sf, sherpa_onnx
sys.argv=['x']
import importlib.util
spec=importlib.util.spec_from_file_location('cs','cmdstudy2c.py'); 
M = "../sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01"
def ff(x, sr, af):
    p = subprocess.run(["ffmpeg","-nostdin","-v","error","-f","f32le","-ar",str(sr),"-ac","1","-i","pipe:0","-af",af,"-ar","16000","-f","f32le","pipe:1"],input=x.astype(np.float32).tobytes(),capture_output=True); return np.frombuffer(p.stdout,dtype=np.float32)
rng=np.random.default_rng(1)
def sp(lines, thr):
    open("tx_kw.txt","w").write("\n".join(lines)+"\n"); c="chunk-16-left-64.int8.onnx"
    return sherpa_onnx.KeywordSpotter(tokens=f"{M}/tokens.txt",encoder=f"{M}/encoder-epoch-12-avg-2-{c}",decoder=f"{M}/decoder-epoch-12-avg-2-{c}",joiner=f"{M}/joiner-epoch-12-avg-2-{c}",keywords_file="tx_kw.txt",keywords_threshold=thr,keywords_score=1.0,num_threads=1,provider="cpu")
def run(s, x):
    st=s.create_stream(); n=0
    for i in range(0,len(x),1600):
        st.accept_waveform(16000,x[i:i+1600])
        while s.is_ready(st):
            s.decode_stream(st)
            if s.get_result(st): n+=1; s.reset_stream(st)
    st.accept_waveform(16000,np.zeros(8000,dtype=np.float32)); st.input_finished()
    while s.is_ready(st):
        s.decode_stream(st)
        if s.get_result(st): n+=1; s.reset_stream(st)
    return n
V=["▁TO MA T O @TOMATO"]; W=V+["▁TO M AT O @TOMATO","▁T OM AT O @TOMATO"]
cfg={"A cur 0.25":(V,0.25),"B variants 0.25":(W,0.25),"C variants 0.15":(W,0.15),"D variants :1.5 0.15":([l.replace(" @", " :1.5 @") for l in W],0.15),"E cur 0.15":(V,0.15)}
clips=[]
import glob
for f in sorted(glob.glob("raw7/*.wav")):
    x,sr=sf.read(f,dtype="float32"); pron=f.split("__")[1]
    b=np.concatenate([ff(x,sr,"anull"),np.zeros(8000,dtype=np.float32)])
    for c,y in {"normal":b,"quiet":b*0.15,"noisy":b+rng.standard_normal(len(b)).astype(np.float32)*0.03,"fast":np.concatenate([ff(x,sr,"atempo=1.4"),np.zeros(8000,dtype=np.float32)])}.items(): clips.append((pron,c,y))
fa=np.load("fa16.npy",mmap_mode="r"); seg=np.asarray(fa[-16000*3600:])   # last hour: human + agent speech
for name,(lines,thr) in cfg.items():
    s=sp(lines,thr); res={}
    for pron,c,y in clips: r=res.setdefault(pron,[0,0]); r[0]+=run(s,y)>0; r[1]+=1
    fah=run(sp(lines,thr),seg)
    print(name, {k:round(v[0]/v[1],2) for k,v in res.items()}, "FA in last hour:", fah, flush=True)
