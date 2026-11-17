"""Cancel-word study 2: standalone word after a pause, as the app hears it (AGC, continuous stream + an onset-primed second stream), 7 voices x 7 conditions; false accepts over 4.9 h with the same two-stream setup."""
import sys, json, subprocess, numpy as np, soundfile as sf, sherpa_onnx
M = "../sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01"
PH = json.load(open("phrases9.json")); VOICES = ["af_heart","am_adam","am_michael","af_bella","bf_emma","bm_george","af_sarah"]
CONFIGS = [(0.25, 0.0), (0.15, 0.0), (0.1, 2.0)]   # (threshold, boost)
rng = np.random.default_rng(7)
def ff(x, sr, af):
    p = subprocess.run(["ffmpeg","-nostdin","-v","error","-f","f32le","-ar",str(sr),"-ac","1","-i","pipe:0","-af",af,"-ar","16000","-f","f32le","pipe:1"],input=x.astype(np.float32).tobytes(),capture_output=True); return np.frombuffer(p.stdout,dtype=np.float32)
def spotter(words, thr, boost, tag):
    open(f"c9_{tag}_raw.txt","w").write("\n".join(w.upper() for w in words)+"\n")
    subprocess.run(["../.venv/bin/sherpa-onnx-cli","text2token","--tokens",f"{M}/tokens.txt","--tokens-type","bpe","--bpe-model",f"{M}/bpe.model",f"c9_{tag}_raw.txt",f"c9_{tag}_tok.txt"],check=True,capture_output=True)
    lines=[f"{t.strip()} {(':%s ' % boost) if boost else ''}@{w.upper().replace(' ','_')}" for t,w in zip(open(f"c9_{tag}_tok.txt").read().strip().split("\n"),words)]
    open(f"c9_{tag}_kw.txt","w").write("\n".join(lines)+"\n"); c="chunk-16-left-64.int8.onnx"
    return sherpa_onnx.KeywordSpotter(tokens=f"{M}/tokens.txt",encoder=f"{M}/encoder-epoch-12-avg-2-{c}",decoder=f"{M}/decoder-epoch-12-avg-2-{c}",joiner=f"{M}/joiner-epoch-12-avg-2-{c}",keywords_file=f"c9_{tag}_kw.txt",keywords_threshold=thr,keywords_score=1.0,num_threads=1,provider="cpu")
def agc(x, st):
    out=[]; 
    for i in range(0,len(x),683):
        c=x[i:i+683]; st[0]=max(np.abs(c).max() if len(c) else 0, st[0]*0.9985); g=max(1,min(12,0.3/max(st[0],0.01))); out.append(np.clip(c*g,-1,1))
    return np.concatenate(out)
def live(sp, y, onset_hint=None):
    """Feeds y (16 kHz) like the app: AGC, continuous stream, plus a primed stream from the first energy onset for 4 s. Returns the set of tags heard."""
    y = agc(y, [0.0]); heard = []; main = sp.create_stream(); alt = None; altend = -1; fl = 1600
    e = [np.sqrt((y[i:i+fl]**2).mean()) for i in range(0, len(y)-fl, fl)]
    quiet = 0; onsets=set()
    for k, v in enumerate(e):
        if v > 0.02 and quiet >= 7: onsets.add((k+2)*fl)   # onset ~0.2 s into the word after >= 0.7 s of quiet
        quiet = 0 if v > 0.02 else quiet + 1
    def pump(st, chunk):
        st.accept_waveform(16000, chunk)
        while sp.is_ready(st):
            sp.decode_stream(st); r = sp.get_result(st)
            if r: heard.append(r); sp.reset_stream(st)
    for i in range(0, len(y), fl):
        chunk = y[i:i+fl]
        if i in onsets: alt = sp.create_stream(); altend = i + 64000; pump(alt, y[max(0, i-9600):i+fl])
        elif alt is not None and i < altend: pump(alt, chunk)
        pump(main, chunk)
    pad = np.zeros(8000, dtype=np.float32); main.accept_waveform(16000, pad); main.input_finished()
    while sp.is_ready(main):
        sp.decode_stream(main); r = sp.get_result(main)
        if r: heard.append(r)
    return heard
def conds(x, sr):
    lead = (rng.standard_normal(32000)*0.002).astype(np.float32); trail = (rng.standard_normal(24000)*0.002).astype(np.float32)
    def mk(w): return np.concatenate([lead, w, trail])
    b = ff(x, sr, "anull")
    return {"normal": mk(b), "fast": mk(ff(x,sr,"atempo=1.4")), "slow": mk(ff(x,sr,"atempo=0.8")), "quiet": mk(b*0.1), "noisy": mk(b)+(rng.standard_normal(32000+len(b)+24000)*0.03).astype(np.float32), "pitchup": mk(ff(x,sr,"asetrate=%d,atempo=%.4f"%(int(sr*1.15),1/1.15))), "pitchdown": mk(ff(x,sr,"asetrate=%d,atempo=%.4f"%(int(sr*0.87),1/0.87)))}
def part(job):
    ci = job; thr, boost = CONFIGS[ci]; sp = spotter(PH, thr, boost, ci)
    tp = {p: {} for p in PH}
    for v in VOICES:
        for i, ph in enumerate(PH):
            x, sr = sf.read(f"raw9/{v}__{i:02d}.wav", dtype="float32")
            for c, y in conds(x, sr).items():
                h = [k.replace("_"," ").lower() for k in live(sp, y)]
                d = tp[ph].setdefault(c, [0,0]); d[0] += ph in h; d[1] += 1
    fa = np.load("fa16.npy", mmap_mode="r"); hours = len(fa)/16000/3600; counts = {}
    for i in range(0, len(fa), 16000*600):
        for k in live(sp, np.asarray(fa[i:i+16000*600])): kk=k.replace("_"," ").lower(); counts[kk]=counts.get(kk,0)+1
    return f"{thr}/{boost}", {"tp": {p: {c: v[0]/v[1] for c,v in d.items()} for p,d in tp.items()}, "fa": counts, "hours": hours}
if __name__ == "__main__":
    from multiprocessing import Pool
    with Pool(3) as pool: out = dict(pool.map(part, range(3)))
    json.dump(out, open("cancelstudy.json","w")); print("DONE", flush=True)
