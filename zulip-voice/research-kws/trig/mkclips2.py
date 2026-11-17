import glob, os, sys, numpy as np, soundfile as sf
src = open('study.py').read(); head = src[:src.index('out = {}')] if 'out = {}' in src else src[:src.index('res = {k: {}')]
head = head.replace('import numpy as np, soundfile as sf, sherpa_onnx', 'import numpy as np, soundfile as sf')
exec(head.replace('speech, ssr = sf.read("corpus_speech.wav", dtype="float32"); speech16 = ff(speech, ssr, "anull")', 'speech16 = np.zeros(16000*20, dtype=np.float32)+0.001'))
os.makedirs('/tmp/kwsweb/clips2', exist_ok=True)
for f in sorted(glob.glob('raw/*__attention.wav')):
    v = os.path.basename(f).split('__')[0]; x, sr = sf.read(f, dtype='float32')
    for name, clip in variants(x).items():
        sf.write(f'/tmp/kwsweb/clips2/att_{v}_{name}.wav', clip, 16000)
print(len(glob.glob('/tmp/kwsweb/clips2/*.wav')))
