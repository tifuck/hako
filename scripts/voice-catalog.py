#!/usr/bin/env python3
"""Regenerate libs/core/src/voice_catalog.json from Hugging Face.

Every transcription model transcribe.cpp publishes under `handy-computer`,
each file pinned to the repo commit it was read at (URL) and its SHA-256, so
a download can only ever be the exact file a reviewed commit names.
Run, review the diff, commit. Needs network; python3 stdlib only.
"""

import json
import sys
import urllib.request

API = "https://huggingface.co/api/models"
AUTHOR = "handy-computer"
OUT = "libs/core/src/voice_catalog.json"
# Not speech-to-text: speaker turns or language ID only.
SKIP = {"diar_streaming_sortformer_4spk-v2.1", "lang-id-voxlingua107-ecapa"}
VAD = "silero-vad-v6.2"
QUANT_ORDER = ["Q4_K_M", "Q5_K_M", "Q6_K", "Q8_0", "F16", "F32"]
# Jarvis wake words: sherpa-onnx open-vocabulary keyword spotter (English,
# 3.3M parameters, Apache-2.0). Pinned by hand; the release asset's digest.
KWS = {
    "id": "kws-zipformer-gigaspeech-3.3m",
    "url": "https://github.com/k2-fsa/sherpa-onnx/releases/download/kws-models/sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01.tar.bz2",
    "sha256": "f170013b4716e41b62b9bfd809687c207cef798ef9bc6534d524e17af9b6561a",
    "size": 0,
    "dir": "sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01",
    "encoder": "encoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx",
    "decoder": "decoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx",
    "joiner": "joiner-epoch-12-avg-2-chunk-16-left-64.int8.onnx",
    "tokens": "tokens.txt",
    "pieces": "bpe.model",
    "license": "apache-2.0",
}


def get(url):
    with urllib.request.urlopen(url, timeout=60) as r:
        return json.load(r)


def files(repo, sha):
    out = []
    for f in get(f"{API}/{repo}/tree/{sha}"):
        path = f["path"]
        if not path.endswith(".gguf") or "lfs" not in f:
            continue
        quant = path[:-5].rsplit("-", 1)[-1]
        out.append(
            {
                "quant": quant,
                "url": f"https://huggingface.co/{repo}/resolve/{sha}/{path}",
                "file": path,
                "size": f["lfs"]["size"],
                "sha256": f["lfs"]["oid"],
            }
        )
    rank = {q: i for i, q in enumerate(QUANT_ORDER)}
    out.sort(key=lambda f: rank.get(f["quant"], len(rank)))
    return out


def main():
    models, vad = [], None
    for m in get(f"{API}?author={AUTHOR}&limit=500"):
        repo = m["id"]
        slug = repo.split("/", 1)[1]
        if not slug.endswith("-gguf"):
            continue
        slug = slug[: -len("-gguf")]
        info = get(f"{API}/{repo}")
        card = info.get("cardData") or {}
        tc = card.get("transcribe_cpp") or {}
        sha = info["sha"]
        fs = files(repo, sha)
        if not fs:
            continue
        if slug.lower() == VAD:
            vad = {"id": slug.lower(), "repo": repo, "license": card.get("license", ""), "files": fs}
            continue
        if slug in SKIP or card.get("pipeline_tag") != "automatic-speech-recognition":
            continue
        full = next((f for f in fs if f["quant"] in ("F16", "F32")), None)
        params = None
        if full:
            params = full["size"] // (2 if full["quant"] == "F16" else 4)
        wer = tc.get("wer_librispeech_test_clean") or {}
        rtf = tc.get("rtf_ryzen_4750u") or {}
        models.append(
            {
                "id": slug.lower(),
                "name": slug,
                "repo": repo,
                "base_model": card.get("base_model", ""),
                "license": card.get("license", ""),
                "languages": card.get("language") or [],
                "streaming": bool(tc.get("streaming")),
                "translate": bool(tc.get("translate")),
                "lang_detect": bool(tc.get("lang_detect")),
                "params": params,
                "wer_en": {k.upper(): v for k, v in wer.items()},
                "rtf_cpu": rtf.get("cpu"),
                "files": fs,
            }
        )
        print(slug, file=sys.stderr)
    if vad is None:
        sys.exit("no VAD model found")
    models.sort(key=lambda m: m["id"])
    kws = dict(KWS)
    req = urllib.request.Request(kws["url"], method="HEAD")
    with urllib.request.urlopen(req, timeout=60) as r:
        kws["size"] = int(r.headers["Content-Length"])
    with open(OUT, "w") as f:
        json.dump({"vad": vad, "kws": kws, "models": models}, f, indent=1, sort_keys=True)
        f.write("\n")
    print(f"{len(models)} models -> {OUT}", file=sys.stderr)


if __name__ == "__main__":
    main()
