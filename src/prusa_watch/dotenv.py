"""Minimal .env loader so native runs pick up the same file docker compose uses.

Rules (a compatible subset of docker compose / python-dotenv):
  - blank lines and lines starting with # are ignored; an optional `export ` prefix is allowed
  - KEY=VALUE; VALUE may be wrapped in single or double quotes
  - unquoted values lose a trailing ` # comment`
  - variables already set in the environment win (the shell overrides the file)
  - empty values are skipped, so `${VAR:-default}` in config.yaml still falls back
"""

from __future__ import annotations

import os
import re
from pathlib import Path
from typing import MutableMapping

_LINE = re.compile(r"^\s*(?:export\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.*?)\s*$")


def parse_dotenv(text: str) -> dict[str, str]:
    out: dict[str, str] = {}
    for raw in text.splitlines():
        if not raw.strip() or raw.lstrip().startswith("#"):
            continue
        m = _LINE.match(raw)
        if not m:
            continue
        key, val = m.group(1), m.group(2)
        if len(val) >= 2 and val[0] == val[-1] and val[0] in "'\"":
            val = val[1:-1]
        else:
            val = re.split(r"\s+#", val, maxsplit=1)[0].rstrip()
        out[key] = val
    return out


def load_dotenv(paths: list[str | Path], environ: MutableMapping[str, str] | None = None) -> list[Path]:
    """Load each existing file in `paths` (first file wins per key). Returns the files loaded."""
    environ = os.environ if environ is None else environ
    loaded: list[Path] = []
    seen: set[Path] = set()
    for p in paths:
        path = Path(p).resolve()
        if path in seen or not path.is_file():
            continue
        seen.add(path)
        for key, val in parse_dotenv(path.read_text(encoding="utf-8")).items():
            if val != "" and key not in environ:
                environ[key] = val
        loaded.append(path)
    return loaded
