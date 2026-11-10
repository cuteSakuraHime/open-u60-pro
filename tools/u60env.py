"""Shared, machine-independent settings for the router tools.

Values come from the environment, then from tools/u60.env (git-ignored).
Nothing personal is stored in the repository.
"""
import os
from pathlib import Path
import shutil

ROOT = Path(__file__).resolve().parent
_ENV_FILE = ROOT / "u60.env"


def _load():
    values = {}
    if _ENV_FILE.exists():
        for line in _ENV_FILE.read_text(encoding="utf-8").splitlines():
            line = line.strip()
            if line and not line.startswith("#") and "=" in line:
                key, value = line.split("=", 1)
                values[key.strip()] = value.strip().strip("'\"")
    values.update({k: v for k, v in os.environ.items() if k.startswith("U60_")})
    return values


_VALUES = _load()


def get(key, default=""):
    return _VALUES.get(key, default)


ROUTER_IP = get("U60_ROUTER_IP", "192.168.0.1")
AGENT_URL = get("U60_AGENT_URL", f"http://{ROUTER_IP}:9090")
ADB = get("U60_ADB") or shutil.which("adb") or "adb"
TEST_NUMBER = get("U60_TEST_NUMBER")
WORK_DIR = Path(get("U60_WORK_DIR") or (ROOT / ".work"))
