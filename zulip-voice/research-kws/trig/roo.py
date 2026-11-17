import subprocess, numpy as np, soundfile as sf, sherpa_onnx, urllib.request, json
M="../sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01"
sp=sherpa_onnx.KeywordSpotter(tokens=f"{M}/tokens.txt",encoder=f"{M}/encoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx",decoder=f"{M}/decoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx",joiner=f"{M}/joiner-epoch-12-avg-2-chunk-16-left-64.int8.onnx",keywords_file="../kw.txt",keywords_threshold=0.25,keywords_score=1.0,num_threads=1,provider="cpu")
def tts(t):
    r=urllib.request.urlopen(urllib.request.Request("http://127.0.0.1:8770/tts",data=json.dumps({"text":t}).encode(),headers={"Content-Type":"application/json"})).read(); open('/tmp/_r.wav','wb').write(r); x,sr=sf.read('/tmp/_r.wav',dtype='float32')
    return np.frombuffer(subprocess.run(["ffmpeg","-nostdin","-v","error","-f","f32le","-ar",str(sr),"-ac","1","-i","pipe:0","-ar","16000","-f","f32le","pipe:1"],input=x.tobytes(),capture_output=True).stdout,dtype=np.float32)
for t in ["Roo.","Roo roo roo.","Kangaroo.","The kangaroo hopped away.","Say roo."]:
    x=tts(t); s=sp.create_stream(); hits=[]
    for i in range(0,len(x),1600):
        s.accept_waveform(16000,x[i:i+1600])
        while sp.is_ready(s):
            sp.decode_stream(s); r=sp.get_result(s)
            if r: hits.append(r); sp.reset_stream(s)
    s.accept_waveform(16000,np.zeros(8000,dtype=np.float32)); s.input_finished()
    while sp.is_ready(s):
        sp.decode_stream(s); r=sp.get_result(s)
        if r: hits.append(r); sp.reset_stream(s)
    print(repr(t).ljust(30),hits)
