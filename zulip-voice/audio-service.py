# /// script
# requires-python = ">=3.12"
# dependencies = ["kokoro==0.9.4", "soundfile>=0.13,<0.14", "faster-whisper==1.2.1", "torch==2.9.1+cpu"]
# [[tool.uv.index]]
# name = "pytorch-cpu"
# url = "https://download.pytorch.org/whl/cpu"
# explicit = true
# [tool.uv.sources]
# torch = { index = "pytorch-cpu" }
# ///
"""Local speech worker; the UI server validates browser requests before proxying."""

import io
import json
import os
import subprocess
from collections.abc import Mapping
from http.server import BaseHTTPRequestHandler, HTTPServer

import numpy as np
import soundfile as sf
import torch
from faster_whisper import WhisperModel
from kokoro import KPipeline

MAX_AUDIO_BYTES = 12 * 1024 * 1024
MAX_AUDIO_S = 120
SAMPLE_RATE_HZ = 24000


# 🧑 "Use the server to do STT and TTS instead of the client, i.e., send audio around"
class Speech:
    """Keep both models resident; requests run sequentially to bound resource use."""

    def __init__(self) -> None:
        torch.set_num_threads(4)
        self.tts = KPipeline(lang_code="a", repo_id="hexgrad/Kokoro-82M", device="cpu")
        self.stt = WhisperModel(
            "small.en", device="cpu", compute_type="int8", cpu_threads=4
        )

    def speak(self, text: str) -> bytes | str:
        """Return a mono WAV or an error, without retaining conversation text."""
        chunks = [
            result.audio.numpy()
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

    def transcribe(self, data: bytes) -> dict[str, str] | str:
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
        segments, _ = self.stt.transcribe(
            samples, language="en", beam_size=1, vad_filter=True
        )
        return {"text": " ".join(segment.text.strip() for segment in segments).strip()}


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
                    200, {"ready": True, "tts": "Kokoro-82M", "stt": "Whisper small.en"}
                )
            else:
                self.reply(404, {"error": "Not found."})

        def do_POST(self) -> None:
            if not self.allowed():
                self.reply(403, {"error": "Use the voice UI server."})
                return
            if self.path not in {"/tts", "/stt"}:
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
            try:
                if self.path == "/tts":
                    request = json.loads(data)
                    text = request.get("text") if isinstance(request, dict) else None
                    if not isinstance(text, str) or not 0 < len(text.strip()) <= 2000:
                        self.reply(400, {"error": "Provide 1–2000 characters of text."})
                        return
                    result = speech.speak(text.strip())
                else:
                    result = speech.transcribe(data)
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

    return Handler


if __name__ == "__main__":
    port = int(os.environ.get("VOICE_AUDIO_PORT", "8770"))
    speech = Speech()
    server = HTTPServer(("127.0.0.1", port), handler(speech, port))
    print(f"Speech worker ready on loopback port {port}", flush=True)
    server.serve_forever()
