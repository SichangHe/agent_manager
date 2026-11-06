"""Exercise real TTS and browser-format STT through the running local worker."""

import io
import json
import subprocess
import time
import urllib.error
import urllib.request
import wave

BASE = "http://127.0.0.1:8770"


def request(path: str, data: bytes, content_type: str) -> bytes:
    req = urllib.request.Request(
        BASE + path, data=data, headers={"Content-Type": content_type}
    )
    with urllib.request.urlopen(req, timeout=120) as response:
        return response.read()


def main() -> None:
    text = "Please ask the manager to review my presentation tomorrow."
    start_s = time.monotonic()
    audio = request("/tts", json.dumps({"text": text}).encode(), "application/json")
    with wave.open(io.BytesIO(audio)) as wav:
        assert wav.getframerate() == 24000
        duration_s = wav.getnframes() / wav.getframerate()
        assert duration_s > 1
    print(f"TTS: {time.monotonic() - start_s:.2f}s for {duration_s:.2f}s speech")
    for fmt, codec, extra in [
        ("mp4", "aac", ["-movflags", "frag_keyframe+empty_moov"]),
        ("webm", "libopus", []),
    ]:
        encoded = subprocess.run(
            [
                "ffmpeg",
                "-v",
                "error",
                "-i",
                "pipe:0",
                "-c:a",
                codec,
                *extra,
                "-f",
                fmt,
                "pipe:1",
            ],
            input=audio,
            capture_output=True,
            check=True,
            timeout=30,
        ).stdout
        start_s = time.monotonic()
        result = json.loads(request("/stt", encoded, f"audio/{fmt}"))
        assert "manager" in result["text"].lower(), result
        assert "presentation" in result["text"].lower(), result
        print(f"{fmt} STT: {time.monotonic() - start_s:.2f}s; {result['text']}")
    for path, data in [("/tts", b'{"text":""}'), ("/stt", b"not audio")]:
        try:
            request(path, data, "application/octet-stream")
        except urllib.error.HTTPError as error:
            assert error.code in {400, 422}
        else:
            raise AssertionError("Invalid input accepted")
    req = urllib.request.Request(
        BASE + "/health", headers={"Origin": "http://unrelated.example"}
    )
    try:
        urllib.request.urlopen(req, timeout=5)
    except urllib.error.HTTPError as error:
        assert error.code == 403
    else:
        raise AssertionError("Direct browser request accepted")


if __name__ == "__main__":
    main()
