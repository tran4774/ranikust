"""Prints the image config of an OCI layout, or checks it.

    oci-config.py LAYOUT            print the whole config (JSON)
    oci-config.py LAYOUT EXPECTED   exit 1 unless the config contains
                                    EXPECTED (JSON): objects compare as
                                    subsets, everything else must be equal

Used by the quick smoke cases (tests/smoke/cases.bash).
"""

import json
import sys
from pathlib import Path


def blob(layout: Path, digest: str) -> dict:
    algo, hexa = digest.split(":", 1)
    return json.loads((layout / "blobs" / algo / hexa).read_text())


def image_config(layout: Path) -> dict:
    index = json.loads((layout / "index.json").read_text())
    manifest = blob(layout, index["manifests"][0]["digest"])
    return blob(layout, manifest["config"]["digest"])


def contains(got, want, path="") -> list[str]:
    if isinstance(want, dict):
        if not isinstance(got, dict):
            return [f"{path or '.'}: got {json.dumps(got)}, want an object"]
        errs = []
        for k, v in want.items():
            if k not in got:
                errs.append(f"{path}.{k}: missing, want {json.dumps(v)}")
            else:
                errs += contains(got[k], v, f"{path}.{k}")
        return errs
    if got != want:
        return [f"{path or '.'}: got {json.dumps(got)}, want {json.dumps(want)}"]
    return []


def main() -> int:
    config = image_config(Path(sys.argv[1]))
    if len(sys.argv) == 2:
        print(json.dumps(config, indent=1, sort_keys=True))
        return 0
    errs = contains(config, json.loads(sys.argv[2]))
    for e in errs:
        print(f"config{e}", file=sys.stderr)
    return 1 if errs else 0


if __name__ == "__main__":
    sys.exit(main())
