#!/usr/bin/env python3
"""Minimal smoke test for Bailian/DashScope Qwen3-ASR-Flash.

This script sends a local audio file to Alibaba Cloud Bailian's OpenAI
compatible endpoint using a base64 data URL. It is intended as a quick way to
verify that short WeChat voice files can be transcribed before integrating ASR
into wx-cli itself.
"""

from __future__ import annotations

import argparse
import base64
import json
import mimetypes
import os
import sys
import urllib.error
import urllib.request
from pathlib import Path


DEFAULT_BASE_URL = "https://dashscope.aliyuncs.com/compatible-mode/v1"
DEFAULT_MODEL = "qwen3-asr-flash"
MAX_FLASH_BYTES = 10 * 1024 * 1024
DEFAULT_AIHUB_PROVIDER = "aliyun_bailian_main"
AIHUB_CONFIG_CANDIDATES = [
    Path.home() / ".config" / "aihub" / "aihub.json",
    Path.home() / "Library" / "Application Support" / "AIHub" / "aihub.json",
]
MIME_BY_EXT = {
    ".aac": "audio/aac",
    ".aiff": "audio/aiff",
    ".amr": "audio/amr",
    ".flac": "audio/flac",
    ".m4a": "audio/mp4",
    ".mp3": "audio/mpeg",
    ".ogg": "audio/ogg",
    ".opus": "audio/opus",
    ".wav": "audio/wav",
    ".webm": "audio/webm",
    ".wma": "audio/x-ms-wma",
}


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=(
            "Send a local audio file to Bailian/DashScope "
            "Qwen3-ASR-Flash for speech-to-text verification."
        )
    )
    parser.add_argument("audio", help="Local audio file path, such as a WeChat .amr file")
    parser.add_argument(
        "--base-url",
        default=None,
        help=(
            "OpenAI compatible base URL. If omitted, try AI Hub provider first, "
            "then fall back to Beijing compatible-mode."
        ),
    )
    parser.add_argument(
        "--model",
        default=DEFAULT_MODEL,
        help="Model name. Default: qwen3-asr-flash",
    )
    parser.add_argument(
        "--api-key-env",
        default="DASHSCOPE_API_KEY",
        help="Environment variable that stores the API key. Default: DASHSCOPE_API_KEY",
    )
    parser.add_argument(
        "--aihub-config",
        help="Optional AI Hub config path. If omitted, common local AI Hub paths are tried",
    )
    parser.add_argument(
        "--provider",
        default=DEFAULT_AIHUB_PROVIDER,
        help="Provider id inside AI Hub config. Default: aliyun_bailian_main",
    )
    parser.add_argument(
        "--language",
        help="Optional language hint, such as zh or en",
    )
    parser.add_argument(
        "--enable-itn",
        action="store_true",
        help="Enable inverse text normalization for Chinese or English audio",
    )
    parser.add_argument(
        "--system-text",
        help="Optional context text for domain terms or entity biasing",
    )
    parser.add_argument(
        "--timeout",
        type=int,
        default=120,
        help="HTTP timeout in seconds. Default: 120",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="Print raw JSON response instead of a compact summary",
    )
    return parser


def fail(message: str) -> int:
    print(f"Error: {message}", file=sys.stderr)
    return 1


def guess_mime_type(path: Path) -> str:
    ext = path.suffix.lower()
    if ext in MIME_BY_EXT:
        return MIME_BY_EXT[ext]

    guessed, _ = mimetypes.guess_type(path.name)
    if guessed:
        return guessed

    return "application/octet-stream"


def build_data_uri(path: Path) -> str:
    mime_type = guess_mime_type(path)
    raw = path.read_bytes()
    encoded = base64.b64encode(raw).decode("ascii")
    return f"data:{mime_type};base64,{encoded}"


def build_payload(args: argparse.Namespace, data_uri: str) -> dict:
    messages = []
    if args.system_text:
        messages.append(
            {
                "role": "system",
                "content": [{"text": args.system_text}],
            }
        )

    messages.append(
        {
            "role": "user",
            "content": [
                {
                    "type": "input_audio",
                    "input_audio": {"data": data_uri},
                }
            ],
        }
    )

    payload = {
        "model": args.model,
        "messages": messages,
        "stream": False,
    }

    asr_options = {}
    if args.language:
        asr_options["language"] = args.language
    if args.enable_itn:
        asr_options["enable_itn"] = True
    if asr_options:
        payload["asr_options"] = asr_options

    return payload


def discover_aihub_config(explicit_path: str | None) -> Path | None:
    if explicit_path:
        path = Path(explicit_path).expanduser().resolve()
        return path if path.exists() else None

    for path in AIHUB_CONFIG_CANDIDATES:
        if path.exists():
            return path
    return None


def load_aihub_provider(config_path: Path, provider_id: str) -> dict:
    data = json.loads(config_path.read_text(encoding="utf-8"))
    providers = data.get("catalog", {}).get("providers", {})
    provider = providers.get(provider_id)
    if not isinstance(provider, dict):
        raise RuntimeError(f"provider not found in AI Hub config: {provider_id}")
    return provider


def to_compatible_base_url(base_url: str | None) -> str:
    if not base_url:
        return DEFAULT_BASE_URL

    normalized = base_url.rstrip("/")
    if normalized.endswith("/compatible-mode/v1"):
        return normalized
    if normalized.endswith("/api/v1"):
        return normalized[:-len("/api/v1")] + "/compatible-mode/v1"
    if normalized.endswith("/v1"):
        return normalized
    return normalized + "/compatible-mode/v1"


def resolve_credentials(args: argparse.Namespace) -> tuple[str, str, str]:
    api_key = os.getenv(args.api_key_env)
    if api_key:
        base_url = args.base_url or DEFAULT_BASE_URL
        return api_key, base_url, f"env:{args.api_key_env}"

    config_path = discover_aihub_config(args.aihub_config)
    if not config_path:
        raise RuntimeError(
            f"{args.api_key_env} is not set and no AI Hub config was found."
        )

    provider = load_aihub_provider(config_path, args.provider)
    api_key = str(provider.get("api_key") or "").strip()
    if not api_key:
        raise RuntimeError(
            f"provider '{args.provider}' in {config_path} does not contain api_key"
        )

    provider_base_url = str(provider.get("base_url") or "").strip()
    base_url = args.base_url or to_compatible_base_url(provider_base_url)
    source = f"aihub:{config_path}:{args.provider}"
    return api_key, base_url, source


def post_json(url: str, api_key: str, payload: dict, timeout: int) -> dict:
    body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
    request = urllib.request.Request(
        url,
        data=body,
        method="POST",
        headers={
            "Authorization": f"Bearer {api_key}",
            "Content-Type": "application/json",
        },
    )

    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return json.loads(response.read().decode("utf-8"))
    except urllib.error.HTTPError as exc:
        detail = exc.read().decode("utf-8", errors="replace")
        raise RuntimeError(f"HTTP {exc.code}: {detail}") from exc
    except urllib.error.URLError as exc:
        raise RuntimeError(f"network error: {exc}") from exc


def print_compact_result(response: dict) -> None:
    choices = response.get("choices") or []
    if not choices:
        print(json.dumps(response, ensure_ascii=False, indent=2))
        return

    message = choices[0].get("message") or {}
    content = message.get("content", "")
    annotations = message.get("annotations") or []
    usage = response.get("usage") or {}
    audio_info = next(
        (item for item in annotations if item.get("type") == "audio_info"),
        {},
    )

    print("Text:")
    print(content)

    language = audio_info.get("language")
    emotion = audio_info.get("emotion")
    if language or emotion:
        print("")
        if language:
            print(f"Language: {language}")
        if emotion:
            print(f"Emotion: {emotion}")

    if usage:
        seconds = usage.get("seconds")
        prompt = usage.get("prompt_tokens")
        completion = usage.get("completion_tokens")
        total = usage.get("total_tokens")
        print("")
        if seconds is not None:
            print(f"Audio seconds: {seconds}")
        if prompt is not None:
            print(f"Prompt tokens: {prompt}")
        if completion is not None:
            print(f"Completion tokens: {completion}")
        if total is not None:
            print(f"Total tokens: {total}")


def main() -> int:
    args = build_parser().parse_args()

    audio_path = Path(args.audio).expanduser().resolve()
    if not audio_path.exists():
        return fail(f"audio file not found: {audio_path}")
    if not audio_path.is_file():
        return fail(f"audio path is not a file: {audio_path}")

    try:
        api_key, base_url, source = resolve_credentials(args)
    except RuntimeError as exc:
        return fail(
            str(exc)
            + f"\nExample:\n  export {args.api_key_env}=sk-xxxx"
        )

    size = audio_path.stat().st_size
    if size > MAX_FLASH_BYTES:
        return fail(
            "audio file exceeds 10 MB, which is outside the recommended "
            "Qwen3-ASR-Flash limit. Use qwen3-asr-flash-filetrans instead."
        )

    data_uri = build_data_uri(audio_path)
    payload = build_payload(args, data_uri)
    endpoint = base_url.rstrip("/") + "/chat/completions"

    print(
        f"Using credential source: {source}",
        file=sys.stderr,
    )
    print(
        f"Calling: {endpoint}",
        file=sys.stderr,
    )

    try:
        response = post_json(endpoint, api_key, payload, args.timeout)
    except RuntimeError as exc:
        return fail(str(exc))

    if args.json:
        print(json.dumps(response, ensure_ascii=False, indent=2))
    else:
        print_compact_result(response)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
