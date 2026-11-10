import numpy as np, soundfile as sf, glob, subprocess, sherpa_onnx, collections
M="../sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01"; c="chunk-16-left-64.int8.onnx"
r=sherpa_onnx.OnlineRecognizer.from_transducer(tokens=f"{M}/tokens.txt",encoder=f"{M}/encoder-epoch-12-avg-2-{c}",decoder=f"{M}/decoder-epoch-12-avg-2-{c}",joiner=f"{M}/joiner-epoch-12-avg-2-{c}",num_threads=1,sample_rate=16000,feature_dim=80,decoding_method="greedy_search")
def ff(x,sr):
    p=subprocess.run(["ffmpeg","-nostdin","-v","error","-f","f32le","-ar",str(sr),"-ac","1","-i","pipe:0","-ar","16000","-f","f32le","pipe:1"],input=x.astype(np.float32).tobytes(),capture_output=True); return np.frombuffer(p.stdout,dtype=np.float32)
cnt=collections.Counter()
for f in sorted(glob.glob("raw8/*.wav")):
    x,sr=sf.read(f,dtype="float32"); y=np.concatenate([np.zeros(8000,dtype=np.float32),ff(x,sr),np.zeros(16000,dtype=np.float32)])
    s=r.create_stream(); s.accept_waveform(16000,y); s.input_finished()
    while r.is_ready(s): r.decode_stream(s)
    res=r.get_result_all(s); t=res.text; toks=res.tokens
    cnt[(f.split('__')[1],t.strip().upper(), tuple(toks) if toks else None)]+=1
for k,v in cnt.most_common(): print(v,k)
