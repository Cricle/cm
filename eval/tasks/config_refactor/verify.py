import os
import subprocess
import sys
from pathlib import Path

workspace = Path(sys.argv[1]).resolve()
env = dict(os.environ)
env["PYTHONPATH"] = str(workspace)
result = subprocess.run(
    [sys.executable, "-m", "unittest", "discover", "-s", "tests", "-v"],
    cwd=workspace,
    env=env,
    check=False,
)
raise SystemExit(result.returncode)
