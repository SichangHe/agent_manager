import json
d=json.load(open('study.json')); K=d['kws']; W=d['whisper']
vs=['normal','fast','quiet','room','doubletalk','noisy']
print(f"{'phrase':12}"+''.join(f'{v:>10}' for v in vs)+'   kws  asr  FAspeech FAnoise   (cells: kws%/asr%, 7 voices)')
rows=[]
for k in K:
    if not all(v in K[k] for v in vs): continue
    kr=[sum(K[k][v])/len(K[k][v]) for v in vs]; wr=[sum(W[k][v])/len(W[k][v]) for v in vs]
    rows.append((k,kr,wr,K[k]['_fa_speech'],K[k]['_fa_noise']))
for k,kr,wr,fs,fn in sorted(rows,key=lambda r:(r[3]+r[4],-sum(r[1]))):
    print(f"{k:12}"+''.join(f'{x*100:4.0f}/{y*100:<4.0f} ' for x,y in zip(kr,wr))+f" {sum(kr)/6*100:4.0f}% {sum(wr)/6*100:4.0f}%  {fs:5d}   {fn:5d}")
