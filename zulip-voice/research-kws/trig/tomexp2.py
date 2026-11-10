import sys, numpy as np, soundfile as sf, glob
sys.argv=['x']
exec(open('tomexp.py').read().split("V=[")[0])
V=["▁TO MA T O @TOMATO"]
clips=[]
for f in sorted(glob.glob("raw8/*.wav")):
    x,sr=sf.read(f,dtype="float32"); b=np.concatenate([ff(x,sr,"anull"),np.zeros(8000,dtype=np.float32)])
    pad=lambda y: np.concatenate([np.zeros(16000,dtype=np.float32),y])
    for c,y in {"normal":b,"quiet":b*0.05,"verylow":b*0.02,"noisy":b+rng.standard_normal(len(b)).astype(np.float32)*0.03,"fast":np.concatenate([ff(x,sr,"atempo=1.5"),np.zeros(8000,dtype=np.float32)]),"pitchdown":np.concatenate([ff(x,sr,"asetrate=%d,atempo=%.4f"%(int(sr*0.8),1/0.8)),np.zeros(8000,dtype=np.float32)])}.items(): clips.append((c,pad(y)))
fa=np.load("fa16.npy",mmap_mode="r"); seg=np.asarray(fa[-16000*3600:])
for thr in (0.05,0.1,0.15,0.25):
    s=sp(V,thr); res={}
    for c,y in clips: r=res.setdefault(c,[0,0]); r[0]+=run(s,y)>0; r[1]+=1
    print(thr,{k:round(v[0]/v[1],2) for k,v in res.items()},"FA last hour:",run(sp(V,thr),seg),flush=True)
