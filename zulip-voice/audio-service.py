# /// script
# requires-python = ">=3.12"
# dependencies = ["kokoro==0.9.4", "soundfile>=0.13,<0.14", "faster-whisper==1.2.1", "torch==2.9.1+cu128", "sherpa-onnx", "transformers>=5.13", "accelerate"]
# [[tool.uv.index]]
# name = "pytorch-cuda"
# url = "https://download.pytorch.org/whl/cu128"
# explicit = true
# [tool.uv.sources]
# torch = { index = "pytorch-cuda" }
# ///
"""Local speech worker; the UI server validates browser requests before proxying."""

import io
import json
import os
import subprocess
import urllib.parse
import time
from collections.abc import Mapping
from http.server import BaseHTTPRequestHandler, HTTPServer

import json as _json
from pathlib import Path

import numpy as np
import soundfile as sf
import torch
from faster_whisper import WhisperModel
from kokoro import KPipeline

KWS_THRESHOLD = 0.25   # measured in research-kws: 12/12 "pause" clips hit, 0/3 noise
KWS_MODEL = "sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01"
MAX_AUDIO_BYTES = 12 * 1024 * 1024
MAX_AUDIO_S = 120
SAMPLE_RATE_HZ = 24000


# 🧑 "Use the server to do STT and TTS instead of the client, i.e., send audio around"; "It must not merely run. It has to be performant and run smoothly"
class Speech:
    """Own one warmed GPU model per process so dictation never queues behind TTS."""

    def __init__(self) -> None:
        torch.set_num_threads(4)
        self.kind = os.environ.get("VOICE_AUDIO_KIND", "tts")
        if self.kind not in {"tts", "stt"}:
            raise ValueError("VOICE_AUDIO_KIND must be tts or stt")
        if not torch.cuda.is_available():
            raise RuntimeError("CUDA is required for speech")
        self.model = os.environ.get("VOICE_STT_MODEL", "small.en")
        # 🧑 "I think Whisper is garbage. It hallucinates a lot. Try to find other better... Open source."
        self.engine = os.environ.get("VOICE_STT_ENGINE", "whisper")
        if self.engine not in {"whisper", "qwen3"}:
            raise ValueError("VOICE_STT_ENGINE must be whisper or qwen3")
        if self.kind == "tts":
            self.tts = KPipeline(
                lang_code="a", repo_id="hexgrad/Kokoro-82M", device="cuda"
            )
            self.speak("Speech is ready. Please choose your conversation.")
        elif self.engine == "qwen3":
            self.load_qwen3()
        else:
            self.stt = WhisperModel(
                self.model, device="cuda", compute_type="float16", cpu_threads=4
            )
            list(
                self.stt.transcribe(
                    np.zeros(16000, dtype=np.float32), language="en", beam_size=1
                )[0]
            )
        self.load_kws()
        torch.cuda.synchronize()

    def speak(self, text: str) -> bytes | str:
        """Return a mono WAV or an error, without retaining conversation text."""
        chunks = [
            result.audio.cpu().numpy()
            for result in self.tts(text, voice="af_heart")
            if result.audio is not None
        ]
        if not chunks:
            return "No speech produced for this text."
        output = io.BytesIO()
        sf.write(
            output,
            np.concatenate(chunks),
            SAMPLE_RATE_HZ,
            format="WAV",
            subtype="PCM_16",
        )
        return output.getvalue()

    def load_qwen3(self) -> None:
        """Qwen3-ASR (Apache-2.0; `VOICE_STT_MODEL` is its HF id, e.g. `Qwen/Qwen3-ASR-0.6B-hf`), warmed; it has no silence rejection, so `transcribe_qwen3` gates it with Whisper's Silero VAD."""
        from transformers import AutoModelForMultimodalLM, AutoProcessor

        self.processor = AutoProcessor.from_pretrained(self.model)
        self.qwen = AutoModelForMultimodalLM.from_pretrained(self.model, dtype=torch.bfloat16, device_map="cuda")
        self.transcribe_qwen3(np.zeros(16000, dtype=np.float32) + 1e-3, force=True)

    def transcribe_qwen3(self, samples: np.ndarray, force: bool = False, prompt: str = "") -> tuple[str, float]:
        """Return `(text, mean token logprob)` of the VAD-trimmed speech; `("", -9.0)` when VAD finds none."""
        from faster_whisper.vad import VadOptions, collect_chunks, get_speech_timestamps

        stamps = get_speech_timestamps(samples, VadOptions())
        if not stamps and not force:
            return "", -9.0
        speech = collect_chunks(samples, stamps)[0] if stamps else samples
        inputs = self.processor.apply_transcription_request(audio=speech, language="English", prompt=prompt or None).to(self.qwen.device, self.qwen.dtype)
        out = self.qwen.generate(**inputs, max_new_tokens=256, do_sample=False, output_scores=True, return_dict_in_generate=True)
        ids = out.sequences[:, inputs["input_ids"].shape[1]:]
        logprobs = self.qwen.compute_transition_scores(out.sequences, out.scores, normalize_logits=True)
        text = self.processor.decode(ids, return_format="transcription_only")[0].strip()
        return text, float(logprobs.mean()) if logprobs.numel() else -9.0

    def load_kws(self) -> None:
        """Keyword spotter for start-word clips; on only when `"kws": true` is in `~/.config/zulip-voice/settings.json` (reversible: set false and restart)."""
        self.kws = None
        try:
            enabled = _json.loads((Path.home() / ".config/zulip-voice/settings.json").read_text()).get("kws") is True
        except (OSError, ValueError):
            enabled = False
        print(f"kws: {'enabled' if enabled and self.kind == 'stt' else 'disabled'} (kind={self.kind}, engine={getattr(self, 'engine', '?')})", flush=True)   # visible in the unit's journal
        if not enabled or self.kind != "stt":
            return
        import sherpa_onnx

        base = Path(os.environ.get("VOICE_KWS_DIR", Path(__file__).parent / "research-kws"))
        model = base / KWS_MODEL
        chunk = "chunk-16-left-64.int8.onnx"
        self.kws = sherpa_onnx.KeywordSpotter(
            tokens=str(model / "tokens.txt"),
            encoder=str(model / f"encoder-epoch-12-avg-2-{chunk}"),
            decoder=str(model / f"decoder-epoch-12-avg-2-{chunk}"),
            joiner=str(model / f"joiner-epoch-12-avg-2-{chunk}"),
            keywords_file=str(base / "kw.txt"),
            keywords_threshold=KWS_THRESHOLD,
            keywords_score=1.0,
            num_threads=1,
            provider="cpu",
        )

    def spot(self, samples: np.ndarray) -> list[str]:
        """Keywords heard in 16 kHz float `samples` (streamed in 100 ms chunks, then 0.5 s of silence to flush)."""
        stream = self.kws.create_stream()
        hits: list[str] = []
        pad = np.zeros(8000, dtype=np.float32)
        for chunk in [*np.array_split(samples, max(1, len(samples) // 1600)), pad]:
            stream.accept_waveform(16000, chunk)
            while self.kws.is_ready(stream):
                self.kws.decode_stream(stream)
                if hit := self.kws.get_result(stream):
                    hits.append(hit)
                    self.kws.reset_stream(stream)
        return hits

    def transcribe(self, data: bytes, prompt: str = "", kws: bool = False) -> dict[str, str] | str:
        """Decode browser MP4/WebM/WAV, rejecting recordings longer than two minutes."""
        try:
            decoded = subprocess.run(
                [
                    "ffmpeg",
                    "-nostdin",
                    "-v",
                    "error",
                    "-protocol_whitelist",
                    "pipe",
                    "-i",
                    "pipe:0",
                    "-t",
                    str(MAX_AUDIO_S + 1),
                    "-ac",
                    "1",
                    "-ar",
                    "16000",
                    "-f",
                    "f32le",
                    "pipe:1",
                ],
                input=data,
                capture_output=True,
                timeout=20,
            )
        except subprocess.TimeoutExpired:
            return "Audio decoding timed out."
        if decoded.returncode:
            return "Cannot decode this audio recording."
        samples = np.frombuffer(decoded.stdout, dtype=np.float32)
        if not samples.size or samples.size > MAX_AUDIO_S * 16000:
            return "Record between a fraction of a second and two minutes."
        if self.engine == "qwen3":
            text, avg_logprob = self.transcribe_qwen3(samples, prompt=prompt)
            heard = self.spot(samples) if kws and self.kws else None
            return {"text": text, "no_speech_prob": 0.0 if text else 1.0, "avg_logprob": avg_logprob, "kws": heard}
        segments, _ = self.stt.transcribe(
            samples,
            language="en",
            beam_size=1,
            vad_filter=True,
            initial_prompt=prompt or None,
        )
        parts = list(segments)
        heard = self.spot(samples) if kws and self.kws else None
        # Quality signals let clients reject hallucinated text on noise: no_speech_prob (max) and avg_logprob (mean) over the segments.
        return {
            "text": " ".join(segment.text.strip() for segment in parts).strip(),
            "no_speech_prob": max((segment.no_speech_prob for segment in parts), default=1.0),
            "avg_logprob": sum(segment.avg_logprob for segment in parts) / len(parts) if parts else -9.0,
            "kws": None if heard is None else heard,
        }


def handler(speech: Speech, port: int) -> type[BaseHTTPRequestHandler]:
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, format: str, *args: object) -> None:
            """Do not log request bodies, transcripts, or URLs."""

        def setup(self) -> None:
            super().setup()
            self.connection.settimeout(30)

        def reply(self, code: int, data: bytes | Mapping[str, object]) -> None:
            body = data if isinstance(data, bytes) else json.dumps(data).encode()
            self.send_response(code)
            self.send_header(
                "Content-Type",
                "audio/wav" if isinstance(data, bytes) else "application/json",
            )
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            self.wfile.write(body)

        def allowed(self) -> bool:
            return self.headers.get(
                "Host"
            ) == f"127.0.0.1:{port}" and not self.headers.get("Origin")

        def do_GET(self) -> None:
            if not self.allowed():
                self.reply(403, {"error": "Use the voice UI server."})
            elif self.path == "/health":
                self.reply(
                    200,
                    {
                        "ready": True,
                        "kind": speech.kind,
                        "device": "cuda",
                        "model": speech.model if speech.kind == "stt" else "Kokoro-82M",
                    },
                )
            else:
                self.reply(404, {"error": "Not found."})

        def do_POST(self) -> None:
            if not self.allowed():
                self.reply(403, {"error": "Use the voice UI server."})
                return
            if self.path != f"/{speech.kind}":
                self.reply(404, {"error": "Not found."})
                return
            try:
                size = int(self.headers.get("Content-Length", "0"))
            except ValueError:
                size = 0
            limit = 16000 if self.path == "/tts" else MAX_AUDIO_BYTES
            if not 0 < size <= limit:
                self.reply(413, {"error": "Missing or oversized audio/text body."})
                return
            data = self.rfile.read(size)
            if len(data) != size:
                self.reply(400, {"error": "Incomplete request body."})
                return
            started_s = time.monotonic()
            try:
                if self.path == "/tts":
                    request = json.loads(data)
                    text = request.get("text") if isinstance(request, dict) else None
                    if not isinstance(text, str) or not 0 < len(text.strip()) <= 2000:
                        self.reply(400, {"error": "Provide 1–2000 characters of text."})
                        return
                    result = speech.speak(text.strip())
                else:
                    prompt = urllib.parse.unquote(self.headers.get("X-Prompt", ""))[:200]
                    result = speech.transcribe(data, prompt, self.headers.get("X-Kws") == "1")
                self.reply(422, {"error": result}) if isinstance(
                    result, str
                ) else self.reply(200, result)
            except (ValueError, RuntimeError, OSError):
                self.reply(
                    503,
                    {
                        "error": "Speech processing failed; retry this recording or sentence."
                    },
                )
            finally:
                print(
                    json.dumps(
                        {
                            "kind": speech.kind,
                            "bytes": size,
                            "duration_ms": round((time.monotonic() - started_s) * 1000),
                        }
                    ),
                    flush=True,
                )

    return Handler


if __name__ == "__main__":
    port = int(os.environ.get("VOICE_AUDIO_PORT", "8770"))
    speech = Speech()
    server = HTTPServer(("127.0.0.1", port), handler(speech, port))
    print(f"Speech worker ready on loopback port {port}", flush=True)
    server.serve_forever()
