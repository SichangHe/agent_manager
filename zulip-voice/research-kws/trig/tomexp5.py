import sys, numpy as np, soundfile as sf, glob
sys.argv=['x']
exec(open('tomexp.py').read().split("V=[")[0])
V=["▁TO MA T O @TOMATO"]
def energy_onset(y):   # first frame (100 ms) whose RMS exceeds 3x the noise floor of the first 0.5 s
    fl=1600; e=[np.sqrt((y[i:i+fl]**2).mean()) for i in range(0,len(y)-fl,fl)]; floor=max(np.median(e[:5]),1e-5)
    for i,v in enumerate(e):
        if v>max(floor*3,0.004): return i*fl
    return 0
def live(s,y,ring_s,resetting):
    st=s.create_stream(); h=0; ring=[]; onset=energy_onset(y)+3200   # onset detected 200 ms into the word
    done=False
    for i in range(0,len(y),1600):
        chunk=y[i:i+1600]; ring.append(chunk)
        if resetting and not done and i>=onset:
            done=True; st=s.create_stream(); back=np.concatenate(ring[-int(ring_s*10):]); st.accept_waveform(16000,back)
        else: st.accept_waveform(16000,chunk)
        while s.is_ready(st):
            s.decode_stream(st)
            if s.get_result(st): h+=1; s.reset_stream(st)
    return h>0
for lead,trail in ((1.0,1.5),(3.0,1.5),(6.0,1.5)):
    for resetting,ring_s in ((False,0),(True,0.6),(True,1.0)):
        s=sp(V,0.25); hit=n=0
        for f in sorted(glob.glob("raw8/*.wav")):
            x,sr=sf.read(f,dtype="float32"); w=ff(x,sr,"anull"); y=np.concatenate([rng.standard_normal(int(16000*lead)).astype(np.float32)*0.002,w,rng.standard_normal(int(16000*trail)).astype(np.float32)*0.002])
            hit+=live(s,y,ring_s,resetting); n+=1
        print("lead",lead,"reset",resetting,ring_s,round(hit/n,2),flush=True)
