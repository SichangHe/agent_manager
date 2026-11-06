# Server speech
(authored by agents unless marked 🧑)

server speech worker
- 🧑 "Use the server to do STT and TTS instead of the client, i.e., send audio around"
- start: `uv run audio-service.py`
  - persistent CPU Kokoro and Whisper small.en models; cached model downloads reused
  - loopback port 8770; override with `VOICE_AUDIO_PORT`
- UI server proxies requests after its Host, Origin and deployment access checks
  - send worker Host `127.0.0.1:8770`; omit browser Origin
  - `GET /health` reports ready after both models load
  - `POST /tts`, JSON `{"text":"One sentence."}`, returns `audio/wav`
    - 2000 characters maximum; use one sentence per request for exact sentence highlighting and seeking
  - `POST /stt`, raw MediaRecorder bytes, returns `{"text":"recognized draft"}`
    - MP4, WebM and WAV decoded by FFmpeg; ≤12 MiB and ≤120 seconds
    - English model; blank transcript means no recognized speech
  - errors return JSON `{"error":"..."}` with non-success HTTP status
- browser records audio and plays returned WAV; no client speech recognition/synthesis needed
  - browser microphone still requires a secure context, including localhost
  - iOS playback still requires a user gesture and actual device validation
- requests run sequentially; proxy should bound its queue and use a 120-second timeout
  - client cancellation must not mark a message read
  - transcription only creates a draft; existing send review remains responsible for recipient and submission
- audio and text stay in memory; no request/content logs or disk audio cache
- validate: `python3 audio-test.py` against the running worker
  - real Kokoro speech encoded into MP4/AAC and WebM/Opus, then transcribed with Whisper
  - measured locally: 4.05 seconds of speech generated in 1.46 seconds; MP4/WebM transcription took 1.67/1.45 seconds
  - both returned the exact synthetic sentence; this does not validate human accents, phone microphone, or iOS playback
