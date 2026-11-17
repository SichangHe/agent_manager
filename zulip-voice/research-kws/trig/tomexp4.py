import sys, numpy as np, soundfile as sf, glob
sys.argv=['x']
exec(open('tomexp.py').read().split("V=[")[0])
V=["▁TO MA T O @TOMATO"]
for lead in (0.0,1.0):
  for trail in (0.5,1.5,3.0):
    s=sp(V,0.25); hit=n=0
    for f in sorted(glob.glob("raw8/*.wav")):
        x,sr=sf.read(f,dtype="float32"); y=np.concatenate([np.zeros(int(16000*lead),dtype=np.float32),ff(x,sr,"anull"),np.zeros(int(16000*trail),dtype=np.float32)])
        # stream without the extra flush: emulate live mic by feeding silence chunks, no input_finished
        st=s.create_stream(); h=0
        for i in range(0,len(y),1600):
            st.accept_waveform(16000,y[i:i+1600])
            while s.is_ready(st):
                s.decode_stream(st)
                if s.get_result(st): h+=1; s.reset_stream(st)
        hit+=h>0; n+=1
    print("lead",lead,"trail",trail,round(hit/n,2),flush=True)
