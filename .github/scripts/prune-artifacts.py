import argparse
import json
import os
import subprocess


def list_items(endpoint, key):
    response = subprocess.check_output(
        ["gh", "api", "--paginate", endpoint, "--jq", f".{key}[] | @json"], text=True
    )
    return [json.loads(line) for line in response.splitlines()]


parser = argparse.ArgumentParser()
parser.add_argument("--repository", default=os.getenv("GITHUB_REPOSITORY"), required=not os.getenv("GITHUB_REPOSITORY"))
parser.add_argument("--before-run", type=int, default=os.getenv("GITHUB_RUN_ID"))
parser.add_argument("--apply", action="store_true")
args = parser.parse_args()
actions = f"repos/{args.repository}/actions"

runs = {
    run["id"]: run
    for run in list_items(f"{actions}/workflows/build.yml/runs?status=completed&per_page=100", "workflow_runs")
    if args.before_run is None or run["id"] < args.before_run
}
artifacts = [
    artifact
    for artifact in list_items(f"{actions}/artifacts?per_page=100", "artifacts")
    if artifact["name"] in {"Synth-linux", "Synth-windows", "Synth-macos"}
    and (artifact.get("workflow_run") or {}).get("id") in runs
]
successful = [
    artifact["workflow_run"]["id"]
    for artifact in artifacts
    if not artifact["expired"] and runs[artifact["workflow_run"]["id"]]["conclusion"] == "success"
]
keep = max(successful, default=None)
obsolete = [artifact for artifact in artifacts if artifact["workflow_run"]["id"] != keep]
size = sum(artifact["size_in_bytes"] for artifact in obsolete)
print(f"Keeping run {keep}. Old artifacts: {len(obsolete)} ({size} bytes).", flush=True)
if args.apply:
    for artifact in obsolete:
        result = subprocess.run(
            ["gh", "api", "--method", "DELETE", f"{actions}/artifacts/{artifact['id']}"],
            capture_output=True, text=True,
        )
        if result.returncode and "(HTTP 404)" not in result.stderr:
            raise SystemExit(result.stderr or f"Could not delete artifact {artifact['id']}")
