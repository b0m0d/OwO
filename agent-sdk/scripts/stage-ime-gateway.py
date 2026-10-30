"""Stage the gateway only after the resource-guarded Cargo build."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess

parser = argparse.ArgumentParser()
parser.add_argument("--repo", required=True, type=Path)
parser.add_argument("--binary", required=True, type=Path)
parser.add_argument("--output", required=True, type=Path)
args = parser.parse_args()
repo = args.repo.resolve()
binary = args.binary.resolve(strict=True)
args.output.mkdir(parents=True, exist_ok=True)
target = args.output / "owo-intent-gateway.exe"
shutil.copy2(binary, target)
sha = lambda path: hashlib.sha256(path.read_bytes()).hexdigest()
assert sha(binary) == sha(target), "Gateway copy integrity mismatch"
sources = {}
for directory in (repo / "gateway", repo / "agent-sdk/crates/owo-agent-protocol"):
    for pattern in ("**/*.rs", "**/Cargo.toml", "Cargo.lock"):
        for source in directory.glob(pattern):
            if "target" not in source.relative_to(directory).parts:
                sources[source.relative_to(repo).as_posix()] = sha(source)
manifest = {
    "commit": subprocess.check_output(["git", "-C", str(repo), "rev-parse", "HEAD"], text=True).strip(),
    "dirty": bool(subprocess.check_output(["git", "-C", str(repo), "status", "--porcelain"], text=True).strip()),
    "sha256": sha(target), "source_files": sources,
}
(args.output / "ime-gateway-build.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
print("Gateway staged and SHA-256 verified")
