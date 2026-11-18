#!/usr/bin/env python3
"""Report which optional translator engines are available on this machine.

Prints a single JSON line. RustMIC calls this from the GUI (and does not
require any of these packages to be installed for the rest of the app).
"""
import importlib.util
import json
import platform
import sys

TTS_LANGUAGES = {
    "kokoro": ["en", "ja", "zh", "fr", "de", "es", "it", "pt", "hi"],
    "chatterbox": ["en"],
    "xtts": [
        "en", "es", "fr", "de", "it", "pt", "tr", "ru", "zh", "ko", "ja",
        "hi", "ar", "nl", "pl", "cs",
    ],
}


def has(name):
    return importlib.util.find_spec(name) is not None


def cuda_available():
    try:
        import torch
        return bool(torch.cuda.is_available())
    except Exception:
        return False


def main():
    result = {
        "python": platform.python_version(),
        "cuda": cuda_available(),
        "faster_whisper": has("faster_whisper"),
        "groq": True,  # uses urllib from the standard library
        "argos": has("argostranslate"),
        "openai": True,  # any OpenAI-compatible endpoint via urllib
        "chatterbox": has("chatterbox"),
        "xtts": has("TTS"),
        "kokoro": has("kokoro"),
        "tts_languages": TTS_LANGUAGES,
        "error": "",
    }
    print(json.dumps(result, ensure_ascii=False))


if __name__ == "__main__":
    try:
        main()
    except Exception as exc:  # pragma: no cover - defensive
        print(json.dumps({"error": str(exc)}))
        sys.exit(1)
