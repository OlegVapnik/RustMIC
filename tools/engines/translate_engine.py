#!/usr/bin/env python3
"""Built-in translation pipeline for RustMIC.

Reads clean voice from RustMIC-voice.sock, runs ASR -> MT -> TTS and writes
synthesised speech back to RustMIC-translate.sock. All PCM is s16le 48 kHz mono.

Dependencies are optional and selected in the RustMIC GUI. Missing packages
are reported on stderr (RustMIC stores it in ~/.cache/RustMIC/engines.log);
the rest of RustMIC keeps working without Python or any of these engines.
"""
import io
import json
import math
import os
import socket
import struct
import sys
import time
import urllib.error
import urllib.request
import wave

RATE = 48000
CHUNK_SAMPLES = 960  # 20 ms
VOICE_THRESHOLD = 350  # about -39 dBFS on s16
SILENCE_MS = 500
MAX_PHRASE_MS = 15000

ASR = os.environ.get("RUSTMIC_ASR", "faster_whisper")
MT = os.environ.get("RUSTMIC_MT", "argos")
TTS = os.environ.get("RUSTMIC_TTS", "kokoro")
SRC_LANG = os.environ.get("RUSTMIC_FROM", "ru")
DST_LANG = os.environ.get("RUSTMIC_TO", "en")
GROQ_KEY = os.environ.get("RUSTMIC_GROQ_KEY", "")
GROQ_MODEL = os.environ.get("RUSTMIC_GROQ_MODEL", "whisper-large-v3")
OPENAI_BASE = os.environ.get("RUSTMIC_OPENAI_BASE", "https://api.openai.com/v1").rstrip("/")
OPENAI_KEY = os.environ.get("RUSTMIC_OPENAI_KEY", "")
OPENAI_MODEL = os.environ.get("RUSTMIC_OPENAI_MODEL", "gpt-4o-mini")
VOICE_SAMPLE = os.environ.get("RUSTMIC_VOICE_SAMPLE", "")
VOICE_SOCK = os.environ.get("RUSTMIC_VOICE_SOCK", "")
TRANSLATE_SOCK = os.environ.get("RUSTMIC_TRANSLATE_SOCK", "")

KOKORO_CODES = {
    "en": "a", "ja": "j", "zh": "z", "fr": "f", "de": "d",
    "es": "e", "it": "i", "pt": "p", "hi": "h",
}


def log(message):
    print(message, file=sys.stderr, flush=True)


class Stream:
    """Unix socket or Windows named pipe, exposed as a binary byte stream."""

    def __init__(self, path, writing):
        self.sock = None
        mode = "wb" if writing else "rb"
        if os.name == "nt":
            self.file = open(path, mode, buffering=0)
        else:
            self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            self.sock.connect(path)
            self.file = self.sock.makefile(mode, buffering=0)

    def read_exact(self, count):
        chunks = []
        remaining = count
        while remaining:
            part = self.file.read(remaining)
            if not part:
                break
            chunks.append(part)
            remaining -= len(part)
        return b"".join(chunks)

    def write(self, data):
        return self.file.write(data)

    def close(self):
        try:
            self.file.close()
        finally:
            if self.sock is not None:
                self.sock.close()


def rms(samples):
    if not samples:
        return 0.0
    return math.sqrt(sum(s * s for s in samples) / len(samples))


def post_json(url, payload, headers=None, timeout=30):
    data = json.dumps(payload).encode("utf-8")
    request = urllib.request.Request(url, data=data, method="POST")
    request.add_header("Content-Type", "application/json")
    for key, value in (headers or {}).items():
        request.add_header(key, value)
    with urllib.request.urlopen(request, timeout=timeout) as response:
        return json.loads(response.read().decode("utf-8"))


def groq_transcribe(pcm):
    boundary = "----RustMICBoundary"
    buffer = io.BytesIO()
    for name, value in (("model", GROQ_MODEL), ("language", SRC_LANG)):
        buffer.write(f"--{boundary}\r\n".encode())
        buffer.write(f'Content-Disposition: form-data; name="{name}"\r\n\r\n'.encode())
        buffer.write(f"{value}\r\n".encode())
    buffer.write(f"--{boundary}\r\n".encode())
    buffer.write(
        'Content-Disposition: form-data; name="file"; filename="voice.wav"\r\n'
        'Content-Type: audio/wav\r\n\r\n'.encode()
    )
    with io.BytesIO() as wav_io:
        with wave.open(wav_io, "wb") as wav:
            wav.setnchannels(1)
            wav.setsampwidth(2)
            wav.setframerate(RATE)
            wav.writeframes(pcm)
        buffer.write(wav_io.getvalue())
    buffer.write(f"\r\n--{boundary}--\r\n".encode())
    request = urllib.request.Request(
        "https://api.groq.com/openai/v1/audio/transcriptions",
        data=buffer.getvalue(), method="POST",
    )
    request.add_header("Content-Type", f"multipart/form-data; boundary={boundary}")
    request.add_header("Authorization", f"Bearer {GROQ_KEY}")
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            return json.loads(response.read().decode("utf-8")).get("text", "")
    except urllib.error.HTTPError as exc:
        if exc.code == 429:
            log("Groq 429 — falling back to local faster-whisper")
            return whisper_transcribe(pcm)
        raise


def whisper_transcribe(pcm):
    from faster_whisper import WhisperModel

    device = "cuda" if cuda() else "cpu"
    model = whisper_transcribe._model
    if model is None or whisper_transcribe._device != device:
        model = WhisperModel("base" if device == "cpu" else "small", device=device)
        whisper_transcribe._model = model
        whisper_transcribe._device = device
    segments, _ = model.transcribe(io.BytesIO(pcm), language=SRC_LANG)
    return " ".join(segment.text.strip() for segment in segments)


whisper_transcribe._model = None
whisper_transcribe._device = None


def cuda():
    try:
        import torch
        return torch.cuda.is_available()
    except Exception:
        return False


def translate_text(text):
    if MT == "openai":
        response = post_json(
            f"{OPENAI_BASE}/chat/completions",
            {
                "model": OPENAI_MODEL,
                "temperature": 0,
                "messages": [
                    {
                        "role": "system",
                        "content": (
                            f"Translate the user's text from {SRC_LANG} to {DST_LANG}. "
                            "Reply with the translation only, no quotes or explanation."
                        ),
                    },
                    {"role": "user", "content": text},
                ],
            },
            {"Authorization": f"Bearer {OPENAI_KEY}"},
        )
        return response["choices"][0]["message"]["content"].strip()
    import argostranslate.translate as argos

    return argos.translate(text, SRC_LANG, DST_LANG)


def read_wav_mono(path):
    with wave.open(path, "rb") as wav:
        rate = wav.getframerate()
        width = wav.getsampwidth()
        channels = wav.getnchannels()
        frames = wav.readframes(wav.getnframes())
    if width != 2:
        raise RuntimeError("voice sample must be 16-bit PCM wav")
    values = struct.unpack("<" + "h" * (len(frames) // 2), frames)
    if channels > 1:
        values = [
            int(sum(values[i:i + channels]) / channels)
            for i in range(0, len(values), channels)
        ]
    return list(values), rate


def resample(values, source_rate):
    if source_rate == RATE:
        return values
    try:
        import numpy as np
        src = np.asarray(values, dtype=np.float32)
        count = int(round(len(src) * RATE / source_rate))
        x = np.linspace(0, len(src) - 1, count)
        return np.interp(x, np.arange(len(src)), src).astype(np.int16).tolist()
    except ImportError:
        ratio = source_rate / RATE
        return [values[min(len(values) - 1, int(i * ratio))] for i in range(int(len(values) / ratio))]


def to_pcm(values):
    return b"".join(struct.pack("<h", max(-32768, min(32767, int(v)))) for v in values)


def synthesize(text):
    device = "cuda" if cuda() else "cpu"
    if TTS == "chatterbox":
        if not VOICE_SAMPLE:
            raise RuntimeError("Chatterbox needs a voice sample in settings")
        from chatterbox.tts import ChatterboxTTS

        model = ChatterboxTTS.from_pretrained(device=device)
        audio = model.generate(text, audio_prompt_path=VOICE_SAMPLE)
        values = audio.squeeze(0).cpu().numpy().tolist() if hasattr(audio, "cpu") else list(audio)
        return resample(values, model.sr)

    if TTS == "xtts":
        if not VOICE_SAMPLE:
            raise RuntimeError("XTTS v2 needs a voice sample in settings")
        from TTS.api import TTS as CoquiTTS

        model = CoquiTTS("tts_models/multilingual/multi-dataset/xtts_v2").to(device)
        path = os.path.join(os.environ.get("RUSTMIC_CACHE", "."), "tts-tmp.wav")
        model.tts_to_file(text=text, speaker_wav=VOICE_SAMPLE, language=DST_LANG, file_path=path)
        values, rate = read_wav_mono(path)
        return resample(values, rate)

    from kokoro import KPipeline

    code = KOKORO_CODES.get(DST_LANG, "a")
    pipeline = KPipeline(lang_code=code)
    parts = []
    for _, audio, _ in pipeline(text, voice="af_heart" if code == "a" else None):
        parts.extend(audio.tolist() if hasattr(audio, "tolist") else list(audio))
    return resample(parts, 24000)


def write_speech(sink, values):
    start = time.monotonic()
    for index in range(0, len(values), CHUNK_SAMPLES):
        chunk = values[index:index + CHUNK_SAMPLES]
        if len(chunk) < CHUNK_SAMPLES:
            chunk += [0] * (CHUNK_SAMPLES - len(chunk))
        sink.write(to_pcm(chunk))
        delay = start + (index // CHUNK_SAMPLES) * 0.02 - time.monotonic()
        if delay > 0:
            time.sleep(delay)


def main():
    if not VOICE_SOCK or not TRANSLATE_SOCK:
        raise RuntimeError("RustMIC did not pass socket paths to the engine")
    source = Stream(VOICE_SOCK, writing=False)
    sink = Stream(TRANSLATE_SOCK, writing=True)
    log(f"engine started: ASR={ASR} MT={MT} TTS={TTS} {SRC_LANG}->{DST_LANG} cuda={cuda()}")
    pending = []
    silence = 0
    while True:
        raw = source.read_exact(CHUNK_SAMPLES * 2)
        if not raw:
            time.sleep(0.02)
            continue
        values = list(struct.unpack("<" + "h" * (len(raw) // 2), raw))
        level = rms(values)
        if level > VOICE_THRESHOLD:
            pending.extend(values)
            silence = 0
        elif pending:
            silence += 20
            pending.extend([0] * CHUNK_SAMPLES)
        if not pending or silence < SILENCE_MS and len(pending) < MAX_PHRASE_MS / 20 * CHUNK_SAMPLES:
            continue
        text = ""
        try:
            text = whisper_transcribe(to_pcm(pending)) if ASR == "faster_whisper" else groq_transcribe(to_pcm(pending))
            if text.strip():
                translated = translate_text(text.strip())
                if translated.strip():
                    write_speech(sink, synthesize(translated))
                    log(f"translated: {text.strip()[:60]!r} -> {translated[:60]!r}")
        except Exception as exc:
            log(f"engine error: {type(exc).__name__}: {exc}")
        pending = []
        silence = 0


if __name__ == "__main__":
    try:
        main()
    except Exception as exc:
        log(f"fatal: {type(exc).__name__}: {exc}")
        sys.exit(1)
