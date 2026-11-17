import sys, numpy as np, soundfile as sf, glob
sys.argv=['x']
exec(open('tomexp.py').read().split("V=[")[0])
clips=[]
for f in sorted(glob.glob("raw8/*.wav")):
    x,sr=sf.read(f,dtype="float32"); b=np.concatenate([np.zeros(16000,dtype=np.float32),ff(x,sr,"anull"),np.zeros(8000,dtype=np.float32)])
    for c,y in {"normal":b,"quiet":b*0.05,"noisy":b+rng.standard_normal(len(b)).astype(np.float32)*0.03}.items(): clips.append((c,y))
fa=np.load("fa16.npy",mmap_mode="r"); seg=np.asarray(fa[-16000*3600:])
def norm(y): p=np.abs(y).max(); return y*(0.5/p) if p>0 else y
for boost in (0.0,1.0,2.0,3.0):
    for thr in (0.1,0.25):
        for nm in (False,True):
            line="▁TO MA T O "+(f":{boost} " if boost else "")+"@TOMATO"
            s=sp([line],thr); res={}
            for c,y in clips: r=res.setdefault(c,[0,0]); r[0]+=run(s,norm(y) if nm else y)>0; r[1]+=1
            fah=run(sp([line],thr),norm(seg[:16000*900]) if nm else seg[:16000*900]) if boost in (0.0,2.0,3.0) else '-'
            print("boost",boost,"thr",thr,"agc" if nm else "raw",{k:round(v[0]/v[1],2) for k,v in res.items()},"FA/15min:",fah,flush=True)
