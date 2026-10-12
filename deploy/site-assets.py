#!/usr/bin/env python3
"""Record this build's site assets and prune assets no recent build references.

Run by deploy/run-benchmark.sh in two steps around the site syncs. The
content-hashed assets are uploaded without --delete, because deleting them at
publish time breaks anyone still holding the previous HTML (an open tab, or HTML
cached for up to the 5-minute max-age), who then requests old hashes that 403.
Instead:

- `record`, after the assets are uploaded and before the HTML that references
  them goes live, writes this build's asset keys to _deploys/<UTC timestamp>.txt
  in the site bucket. Recording first means every build whose HTML went live is
  in a manifest, even if the publish fails before `prune`.
- `prune`, at the end of the publish, keeps the newest --keep manifests, deletes
  the older ones, and deletes every asset key that none of the kept manifests
  (nor the local build) lists.

Pruning is skipped until --keep manifests exist, so assets uploaded before the
first manifest are only removed once --keep builds have been published since.
The un-hashed set (HTML, SEO files, root icons) is not handled here: the
second sync pass owns it with --delete, and it must match that pass's filters.
"""

import argparse
import json
import subprocess
import sys
import tempfile
from datetime import datetime, timezone
from pathlib import Path

MANIFEST_PREFIX = "_deploys/"
# Must match the un-hashed set in run-benchmark.sh's two sync passes.
UNHASHED_NAMES = {"robots.txt", "sitemap.xml", "favicon.svg", "apple-touch-icon.png"}


def is_asset(key: str) -> bool:
    """True for keys in the content-hashed pass-1 key space."""
    if key.startswith(MANIFEST_PREFIX) or key.endswith(".html"):
        return False
    return key not in UNHASHED_NAMES


def aws(*args: str, stdin: str | None = None) -> str:
    result = subprocess.run(
        ["aws", *args], input=stdin, capture_output=True, text=True, check=False
    )
    if result.returncode != 0:
        sys.exit(f"aws {' '.join(args[:2])} failed: {result.stderr.strip()}")
    return result.stdout


def list_keys(bucket: str, region: str) -> list[str]:
    out = aws(
        "s3api", "list-objects-v2", "--bucket", bucket, "--region", region,
        "--query", "Contents[].Key", "--output", "json",
    )
    return json.loads(out) or []


def delete_keys(bucket: str, region: str, keys: list[str]) -> None:
    for i in range(0, len(keys), 1000):
        batch = {"Objects": [{"Key": k} for k in keys[i : i + 1000]], "Quiet": True}
        with tempfile.NamedTemporaryFile("w", suffix=".json") as f:
            json.dump(batch, f)
            f.flush()
            out = aws(
                "s3api", "delete-objects", "--bucket", bucket, "--region", region,
                "--delete", f"file://{f.name}", "--output", "json",
            )
        errors = json.loads(out or "{}").get("Errors", [])
        if errors:
            sys.exit(f"delete-objects reported {len(errors)} errors, first: {errors[0]}")


def local_assets(out_dir: Path) -> list[str]:
    build = sorted(
        key
        for p in out_dir.rglob("*")
        if p.is_file() and is_asset(key := p.relative_to(out_dir).as_posix())
    )
    if not build:
        sys.exit(f"no assets found under {out_dir}; refusing to continue")
    return build


def record(args: argparse.Namespace) -> None:
    build = local_assets(args.out_dir)
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S%fZ")
    manifest_key = f"{MANIFEST_PREFIX}{stamp}.txt"
    aws("s3", "cp", "-", f"s3://{args.bucket}/{manifest_key}", "--region", args.region,
        stdin="\n".join(build) + "\n")
    print(f"wrote {manifest_key} ({len(build)} assets)")


def prune(args: argparse.Namespace) -> None:
    # The local build is always kept, even if its manifest is somehow missing.
    referenced = set(local_assets(args.out_dir))
    keys = list_keys(args.bucket, args.region)
    # Timestamped names sort oldest to newest.
    manifests = sorted(k for k in keys if k.startswith(MANIFEST_PREFIX) and k.endswith(".txt"))
    if len(manifests) < args.keep:
        print(f"{len(manifests)} of {args.keep} manifests recorded; not pruning yet")
        return

    kept, expired = manifests[-args.keep :], manifests[: -args.keep]
    for key in kept:
        listed = aws("s3", "cp", f"s3://{args.bucket}/{key}", "-", "--region", args.region)
        referenced.update(line for line in listed.splitlines() if line)

    stale = sorted(k for k in keys if is_asset(k) and k not in referenced)
    delete_keys(args.bucket, args.region, stale + expired)
    print(f"kept assets of {len(kept)} builds; deleted {len(stale)} assets, {len(expired)} manifests")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("command", choices=["record", "prune"])
    parser.add_argument("--bucket", required=True)
    parser.add_argument("--region", required=True)
    parser.add_argument("--out-dir", required=True, type=Path, help="the built site (site/out-site)")
    parser.add_argument("--keep", type=int, default=5, help="prune: builds whose assets stay (default 5)")
    args = parser.parse_args()
    if args.keep < 1:
        sys.exit("--keep must be at least 1")
    record(args) if args.command == "record" else prune(args)


if __name__ == "__main__":
    main()
