#!/usr/bin/env python3
"""Long-task A/B harness for cm.

Each task is a small workspace plus a verifier. The harness runs an external
agent command twice: once with a seeded cm store and a memory-first prompt,
once without cm. Success is decided by the task verifier, not by the agent's
self-report.

Agent command contract
----------------------
`--agent-cmd` is a shell command template. It is formatted with:

  {cm}         path to the cm binary
  {workspace}  fresh workspace for this run
  {memory}     cm file path (empty string in the without-cm run)
  {prompt}     prompt file path

The agent also receives these environment variables:

  CM_TASK_ID, CM_WORKSPACE, CM_MEMORY, CM_PROMPT, CM_PROMPT_FILE, CM_MODE

For token/turn accounting, the agent may write:

  {"tokens": 1234, "turns": 8, "tool_calls": 12, "files_read": 3}

to `CM_USAGE_FILE` (JSON). Missing fields stay null.

Examples
--------
  python eval/agent_ab.py --cm target/release/cm.exe \
    --agent-cmd "my-agent --prompt {prompt} --cwd {workspace}"

  python eval/agent_ab.py --agent-cmd "python my_agent.py" --runs 3
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path


def find_cm(explicit: str | None) -> str:
    if explicit:
        return explicit
    if os.environ.get("CM"):
        return os.environ["CM"]
    root = Path(__file__).resolve().parents[1]
    for candidate in (
        root / "target" / "release" / "cm.exe",
        root / "target" / "release" / "cm",
        root / "target" / "debug" / "cm.exe",
        root / "target" / "debug" / "cm",
        root / "cm.exe",
        root / "cm",
    ):
        if candidate.exists():
            return str(candidate)
    found = shutil.which("cm")
    if found:
        return found
    raise SystemExit("cm binary not found; pass --cm or set CM")


def run(cmd: list[str], cwd: Path, stdin_path: Path | None = None, timeout: int = 300) -> subprocess.CompletedProcess:
    stdin = stdin_path.open("rb") if stdin_path else None
    try:
        return subprocess.run(
            cmd,
            cwd=cwd,
            stdin=stdin,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=timeout,
            check=False,
        )
    finally:
        if stdin:
            stdin.close()


def must_run(cmd: list[str], cwd: Path, stdin_path: Path | None = None, timeout: int = 300) -> str:
    result = run(cmd, cwd, stdin_path, timeout)
    if result.returncode != 0:
        raise RuntimeError(
            f"command failed ({result.returncode}): {' '.join(cmd)}\n"
            f"{result.stderr.decode('utf-8', 'replace')}"
        )
    return result.stdout.decode("utf-8", "replace")


def load_tasks(eval_dir: Path, selected: list[str] | None) -> list[dict]:
    tasks = []
    for path in sorted((eval_dir / "tasks").glob("*/task.json")):
        task = json.loads(path.read_text(encoding="utf-8"))
        task["_dir"] = path.parent
        if selected and task["id"] not in selected:
            continue
        tasks.append(task)
    if not tasks:
        raise SystemExit("no tasks found")
    return tasks


def seed_memory(cm: str, task: dict, workspace: Path) -> str:
    memory = workspace / "cm"
    must_run([cm, "init", "--file", str(memory), "--empty"], workspace)
    memory_jsonl = task["_dir"] / "memory.jsonl"
    if memory_jsonl.exists():
        must_run([cm, "import", "--file", str(memory)], workspace, memory_jsonl)
    cmd = [cm, "frame", "set", "--file", str(memory), "--task", task["id"], "--goal", task["goal"]]
    for step in task.get("next", []):
        cmd += ["--next", step]
    for constraint in task.get("constraints", []):
        cmd += ["--constraint", constraint]
    for touch in task.get("touches", []):
        cmd += ["--touch", touch]
    must_run(cmd, workspace)
    return str(memory)


def session_prompts(task: dict) -> list[str]:
    sessions = task.get("sessions")
    if not sessions:
        return [task["prompt"]]
    prompts = []
    for session in sessions:
        prompts.append(session["prompt"] if isinstance(session, dict) else session)
    return prompts


def task_prompt(
    cm: str, task: dict, mode: str, memory: str, session_prompt: str, session_count: int
) -> str:
    if mode == "without-cm":
        return session_prompt.strip() + "\n"
    write_rule = (
        "Write one cm record only when a non-obvious decision must survive into a later session. "
        if session_count > 1
        else "Do not write a cm record for a one-shot task. "
    )
    return (
        "You are working in a repository with a cm memory sidecar.\n"
        f"cm binary: {cm}\n"
        f"memory file: {memory}\n\n"
        "At session start run this exactly once:\n"
        f'  "{cm}" brief --file "{memory}" --task current --budget 1200\n\n'
        "Do not call `cm recall` in the same pass unless the brief is missing a "
        "specific fact. "
        f"{write_rule}\n\n"
        "Task:\n"
        f"{session_prompt.strip()}\n"
    )


def copy_task_workspace(task: dict, dest: Path) -> None:
    source = task["_dir"] / "workspace"
    shutil.copytree(source, dest, dirs_exist_ok=True)


def verify_task(task: dict, workspace: Path, timeout: int) -> bool:
    cmd = [sys.executable, str(task["_dir"] / "verify.py"), str(workspace)]
    result = run(cmd, workspace, timeout=timeout)
    return result.returncode == 0


def read_usage(path: Path) -> dict:
    if not path.exists():
        return {}
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return {}
    return value if isinstance(value, dict) else {}


def run_one(cm: str, task: dict, mode: str, run_idx: int, args: argparse.Namespace) -> dict:
    workspace = Path(tempfile.mkdtemp(prefix=f"cm-agent-{task['id']}-{mode}-"))
    try:
        copy_task_workspace(task, workspace)
        memory = seed_memory(cm, task, workspace) if mode == "with-cm" else ""
        prompts = session_prompts(task)
        started = time.perf_counter()
        usage_totals = {"tokens": 0, "turns": 0, "tool_calls": 0, "files_read": 0}
        usage_missing = {key: False for key in usage_totals}
        last_exit = 0
        stdout_tail = ""
        stderr_tail = ""
        for session_idx, session_prompt in enumerate(prompts, 1):
            prompt = task_prompt(cm, task, mode, memory, session_prompt, len(prompts))
            prompt_file = workspace / f".cm-task-prompt-{session_idx}.txt"
            prompt_file.write_text(prompt, encoding="utf-8")
            usage_file = workspace / f".cm-usage-{session_idx}.json"
            env = dict(os.environ)
            env.update(
                {
                    "CM_TASK_ID": task["id"],
                    "CM_WORKSPACE": str(workspace),
                    "CM_MEMORY": memory,
                    "CM_PROMPT": prompt,
                    "CM_PROMPT_FILE": str(prompt_file),
                    "CM_USAGE_FILE": str(usage_file),
                    "CM_MODE": mode,
                    "CM_BIN": cm,
                    "CM_SESSION": str(session_idx),
                    "CM_SESSION_COUNT": str(len(prompts)),
                }
            )
            command = args.agent_cmd.format(
                cm=cm, workspace=workspace, memory=memory, prompt=prompt_file
            )
            agent = subprocess.run(
                command,
                cwd=workspace,
                env=env,
                shell=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=args.timeout,
                check=False,
            )
            last_exit = agent.returncode
            stdout_tail = agent.stdout.decode("utf-8", "replace")[-1000:]
            stderr_tail = agent.stderr.decode("utf-8", "replace")[-1000:]
            usage = read_usage(usage_file)
            for key in usage_totals:
                value = usage.get(key)
                if isinstance(value, (int, float)):
                    usage_totals[key] += value
                else:
                    usage_missing[key] = True
            if agent.returncode != 0:
                break
        elapsed = time.perf_counter() - started
        success = verify_task(task, workspace, args.timeout)
        row = {
            "task": task["id"],
            "mode": mode,
            "run": run_idx,
            "sessions": len(prompts),
            "success": success,
            "agent_exit": last_exit,
            "seconds": round(elapsed, 3),
            "tokens": None if usage_missing["tokens"] else usage_totals["tokens"],
            "turns": None if usage_missing["turns"] else usage_totals["turns"],
            "tool_calls": None if usage_missing["tool_calls"] else usage_totals["tool_calls"],
            "files_read": None if usage_missing["files_read"] else usage_totals["files_read"],
            "workspace": str(workspace) if args.keep else None,
            "stdout_tail": stdout_tail if args.verbose else None,
            "stderr_tail": stderr_tail if args.verbose else None,
        }
        return row
    finally:
        if not args.keep:
            shutil.rmtree(workspace, ignore_errors=True)


def summarize(rows: list[dict]) -> dict:
    summary = {}
    for mode in ("with-cm", "without-cm"):
        selected = [row for row in rows if row["mode"] == mode]
        if not selected:
            continue
        tokens = [row["tokens"] for row in selected if isinstance(row["tokens"], (int, float))]
        turns = [row["turns"] for row in selected if isinstance(row["turns"], (int, float))]
        files = [row["files_read"] for row in selected if isinstance(row["files_read"], (int, float))]
        summary[mode] = {
            "runs": len(selected),
            "success_rate": round(sum(1 for row in selected if row["success"]) / len(selected), 3),
            "median_seconds": round(statistics.median(row["seconds"] for row in selected), 3),
            "median_tokens": round(statistics.median(tokens), 1) if tokens else None,
            "median_turns": round(statistics.median(turns), 1) if turns else None,
            "median_files_read": round(statistics.median(files), 1) if files else None,
        }
    return summary


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--cm", help="path to the cm binary")
    parser.add_argument("--agent-cmd", default=os.environ.get("CM_AGENT_CMD"), help="agent command template")
    parser.add_argument("--tasks", nargs="*", help="task ids to run")
    parser.add_argument("--runs", type=int, default=1)
    parser.add_argument("--timeout", type=int, default=300)
    parser.add_argument("--mode", choices=["both", "with-cm", "without-cm"], default="both")
    parser.add_argument("--keep", action="store_true", help="keep run workspaces")
    parser.add_argument("--verbose", action="store_true")
    parser.add_argument("--json-out", type=Path)
    args = parser.parse_args()
    if not args.agent_cmd:
        raise SystemExit("pass --agent-cmd (or set CM_AGENT_CMD)")

    cm = find_cm(args.cm)
    eval_dir = Path(__file__).resolve().parent
    tasks = load_tasks(eval_dir, args.tasks)
    modes = ["with-cm", "without-cm"] if args.mode == "both" else [args.mode]
    rows = []
    for task in tasks:
        for mode in modes:
            for run_idx in range(1, args.runs + 1):
                print(f"[{task['id']}] {mode} run {run_idx}", file=sys.stderr)
                rows.append(run_one(cm, task, mode, run_idx, args))
    report = {"cm": cm, "tasks": [task["id"] for task in tasks], "rows": rows, "summary": summarize(rows)}
    print(json.dumps(report, ensure_ascii=False, indent=2))
    if args.json_out:
        args.json_out.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main())
