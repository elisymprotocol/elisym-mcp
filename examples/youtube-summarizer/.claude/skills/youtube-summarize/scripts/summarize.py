#!/usr/bin/env python3
"""YouTube transcript extractor.

Usage:
    python3 summarize.py <youtube_url> [--output transcript.json] [--lang en]

Extracts subtitles/transcript from a YouTube video.
Summarization is handled by the LLM (Claude) that calls this script.
"""

import argparse
import json
import os
import re
import subprocess
import sys
import tempfile


def get_video_info(url: str) -> dict:
    """Fetch video title and duration."""
    result = subprocess.run(
        ["yt-dlp", "--dump-json", "--no-download", url],
        capture_output=True, text=True, timeout=30,
    )
    if result.returncode != 0:
        raise RuntimeError(f"yt-dlp failed: {result.stderr.strip()}")
    info = json.loads(result.stdout)
    return {
        "title": info.get("title", "Unknown"),
        "duration": info.get("duration", 0),
        "channel": info.get("channel", "Unknown"),
    }


def fetch_subtitles(url: str, lang: str = "en") -> str | None:
    """Try to get subtitles via yt-dlp."""
    with tempfile.TemporaryDirectory() as tmpdir:
        output_path = os.path.join(tmpdir, "subs")
        subprocess.run(
            [
                "yt-dlp",
                "--write-auto-sub",
                "--write-sub",
                "--sub-lang", lang,
                "--sub-format", "vtt",
                "--skip-download",
                "-o", output_path,
                url,
            ],
            capture_output=True, text=True, timeout=60,
        )
        for f in os.listdir(tmpdir):
            if f.endswith(".vtt"):
                vtt_path = os.path.join(tmpdir, f)
                return parse_vtt(vtt_path)
    return None


def parse_vtt(path: str) -> str:
    """Parse VTT subtitles into plain text, removing duplicates."""
    lines = []
    seen = set()
    with open(path, "r", encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line or line.startswith("WEBVTT") or line.startswith("Kind:") or line.startswith("Language:"):
                continue
            if re.match(r"^\d{2}:\d{2}", line) or "-->" in line:
                continue
            clean = re.sub(r"<[^>]+>", "", line)
            if clean and clean not in seen:
                seen.add(clean)
                lines.append(clean)
    return " ".join(lines)


def transcribe_audio(url: str) -> str:
    """Download audio and transcribe with Whisper (fallback)."""
    try:
        import whisper
    except ImportError:
        print("Error: whisper not installed. Run: pip install openai-whisper", file=sys.stderr)
        sys.exit(1)

    with tempfile.TemporaryDirectory() as tmpdir:
        audio_path = os.path.join(tmpdir, "audio.mp3")
        print("Downloading audio...", file=sys.stderr)
        result = subprocess.run(
            [
                "yt-dlp",
                "-x", "--audio-format", "mp3",
                "--audio-quality", "5",
                "-o", audio_path,
                url,
            ],
            capture_output=True, text=True, timeout=300,
        )
        if result.returncode != 0:
            raise RuntimeError(f"Audio download failed: {result.stderr.strip()}")

        actual_path = audio_path
        if not os.path.exists(actual_path):
            for f in os.listdir(tmpdir):
                if f.startswith("audio"):
                    actual_path = os.path.join(tmpdir, f)
                    break

        print("Transcribing with Whisper...", file=sys.stderr)
        model = whisper.load_model("base")
        result = model.transcribe(actual_path)
        return result["text"]


def main():
    parser = argparse.ArgumentParser(description="Extract transcript from a YouTube video")
    parser.add_argument("url", help="YouTube video URL")
    parser.add_argument("--output", "-o", help="Output file (default: stdout)")
    parser.add_argument("--lang", default="en", help="Subtitle language (default: en)")
    args = parser.parse_args()

    if not re.search(r"(youtube\.com|youtu\.be)", args.url):
        print("Error: not a valid YouTube URL", file=sys.stderr)
        sys.exit(1)

    print("Fetching video info...", file=sys.stderr)
    video_info = get_video_info(args.url)
    print(f"Video: {video_info['title']} ({video_info['duration'] // 60} min)", file=sys.stderr)

    print("Fetching subtitles...", file=sys.stderr)
    transcript = fetch_subtitles(args.url, args.lang)

    if not transcript:
        print("No subtitles found, falling back to Whisper transcription...", file=sys.stderr)
        transcript = transcribe_audio(args.url)

    if not transcript or len(transcript.strip()) < 50:
        print("Error: could not extract transcript from video", file=sys.stderr)
        sys.exit(1)

    print(f"Transcript length: {len(transcript)} chars", file=sys.stderr)

    output = json.dumps({
        "title": video_info["title"],
        "channel": video_info["channel"],
        "duration_min": video_info["duration"] // 60,
        "transcript": transcript,
    }, ensure_ascii=False, indent=2)

    if args.output:
        with open(args.output, "w", encoding="utf-8") as f:
            f.write(output)
        print(f"Saved to {args.output}", file=sys.stderr)
    else:
        print(output)


if __name__ == "__main__":
    main()
