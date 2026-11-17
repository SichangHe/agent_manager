import sys, numpy as np, soundfile as sf, glob
sys.argv=['x']
exec(open('tomexp.py').read().split("V=[")[0])
def box(x, sr_in):
    ratio=sr_in/16000; n=int(len(x)//ratio); out=np.zeros(n,dtype=np.float32)
    for i in range(n):
        a=int(i*ratio); b=max(a+1,int((i+1)*ratio)); out[i]=x[a:b].mean()
    return out
def sinc(x, sr_in): return ff(x, sr_in, "anull")
rng=np.random.default_rng(2)
files=sorted(glob.glob("raw7/*__mayt__*.wav"))[:21]
V=["▁TO MA T O @TOMATO"]
for sr_in in (48000, 44100):
    for name,fn in (("box",box),("sinc",sinc)):
        s=sp(V,0.25); hit=n=0
        for f in files:
            x,sr=sf.read(f,dtype="float32")
            xin=ff(x,sr,"anull") if False else None
            up=np.frombuffer(subprocess.run(["ffmpeg","-nostdin","-v","error","-f","f32le","-ar",str(sr),"-ac","1","-i","pipe:0","-ar",str(sr_in),"-f","f32le","pipe:1"],input=x.astype(np.float32).tobytes(),capture_output=True).stdout,dtype=np.float32)
            y=fn(up,sr_in) if fn is box else ff(up,sr_in,"anull")
            y=np.concatenate([y,np.zeros(8000,dtype=np.float32)]); hit+=run(s,y)>0; n+=1
        print(sr_in,name,round(hit/n,2),flush=True)
