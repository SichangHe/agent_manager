import sys, glob, time, numpy as np, soundfile as sf, sherpa_onnx
M = "sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01"
thr = float(sys.argv[1]) if len(sys.argv) > 1 else 0.25
score = float(sys.argv[2]) if len(sys.argv) > 2 else 1.0
spotter = sherpa_onnx.KeywordSpotter(
    tokens=f"{M}/tokens.txt", encoder=f"{M}/encoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx",
    decoder=f"{M}/decoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx", joiner=f"{M}/joiner-epoch-12-avg-2-chunk-16-left-64.int8.onnx",
    keywords_file="kw.txt", keywords_threshold=thr, keywords_score=score, num_threads=1, provider="cpu")
def spot(path):
    x, sr = sf.read(path, dtype="float32")
    if x.ndim > 1: x = x[:, 0]
    t0 = time.perf_counter(); s = spotter.create_stream(); hits = []
    for i in range(0, len(x), 1600):   # 100 ms chunks, like a live stream
        s.accept_waveform(sr, x[i:i+1600])
        while spotter.is_ready(s):
            spotter.decode_stream(s); r = spotter.get_result(s)
            if r: hits.append(r); spotter.reset_stream(s)
    s.accept_waveform(sr, np.zeros(int(sr * 0.5), dtype=np.float32)); s.input_finished()
    while spotter.is_ready(s):
        spotter.decode_stream(s); r = spotter.get_result(s)
        if r: hits.append(r); spotter.reset_stream(s)
    return hits, (time.perf_counter() - t0) * 1000, len(x) / sr * 1000
rows = {}
for p in sorted(glob.glob("clips/*.wav")):
    hits, ms, dur = spot(p); kind = p.split("/")[1].split("_")[0]
    rows.setdefault(kind, []).append((p.split("/")[1], hits, ms, dur))
for kind, items in rows.items():
    print(f"== {kind}")
    for name, hits, ms, dur in items: print(f"  {name:32} hits={hits}  proc {ms:.0f}ms for {dur:.0f}ms audio")
