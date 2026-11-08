#!/usr/bin/env node
/**
 * Fully automatic cm wiki loop.
 *
 * 1. cm wiki plan
 * 2. cm wiki generate in bounded batches until nothing is pending
 * 3. cm wiki check --strict until clean
 * 4. optional cm wiki remember
 *
 * The agent and reviewer commands receive the page prompt on stdin and must
 * print Markdown / reviewer JSON on stdout, like `cm wiki generate`.
 *
 * Example:
 *   node eval/wiki_auto.mjs \
 *     --cm target/release/cm.exe \
 *     --root C:/wp/empty/pi \
 *     --out C:/wp/empty/wiki-v10 \
 *     --agent-cmd "C:/wp/empty/tools/node-v24.22.1-win-x64/node.exe C:/wp/empty/tools/cm-rs/eval/zcode_agent.mjs" \
 *     --review-cmd  "C:/wp/empty/tools/node-v24.22.1-win-x64/node.exe C:/wp/empty/tools/cm-rs/eval/zcode_agent.mjs" \
 *     --rounds 2 --batch 5 --max-batches 20
 */
import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { resolve } from "node:path";

function usage() {
  console.error(`usage: node eval/wiki_auto.mjs --cm <cm> --root <repo> --out <wiki> --agent-cmd <cmd> --review-cmd <cmd> [options]

options:
  --rounds <n>        review rounds per page (default 2)
  --batch <n>         pages per generate call (default 5)
  --max-batches <n>    stop after n generate calls (default 20)
  --task <id>         only this page id
  --memory <file>     enable wiki remember to this memory file
  --remember          run wiki remember after strict check
  --timeout <ms>      per cm generate call (default 3600000)
  --help`);
}

function parseArgs(argv) {
  const args = {
    rounds: 2,
    batch: 5,
    maxBatches: 20,
    task: null,
    remember: false,
    timeout: 3_600_000,
  };
  for (let i = 0; i < argv.length; i++) {
    const key = argv[i];
    const value = () => {
      if (i + 1 >= argv.length) throw new Error(`${key} requires a value`);
      return argv[++i];
    };
    switch (key) {
      case "--cm": args.cm = value(); break;
      case "--root": args.root = value(); break;
      case "--out": args.out = value(); break;
      case "--agent-cmd": args.agentCmd = value(); break;
      case "--review-cmd": args.reviewCmd = value(); break;
      case "--rounds": args.rounds = Number(value()); break;
      case "--batch": args.batch = Number(value()); break;
      case "--max-batches": args.maxBatches = Number(value()); break;
      case "--task": args.task = value(); break;
      case "--memory": args.memory = value(); break;
      case "--remember": args.remember = true; break;
      case "--timeout": args.timeout = Number(value()); break;
      case "--help": args.help = true; break;
      default: throw new Error(`unknown option ${key}`);
    }
  }
  return args;
}

function run(cm, args, { allowFailure = false, timeout = 3_600_000 } = {}) {
  const result = spawnSync(cm, args, {
    encoding: "utf8",
    maxBuffer: 256 * 1024 * 1024,
    timeout,
  });
  if (result.error) throw result.error;
  if (result.status !== 0 && !allowFailure) {
    throw new Error(`cm ${args.join(" ")} failed (${result.status})\n${result.stderr || result.stdout}`);
  }
  return result.stdout || "";
}

function json(cm, args, options) {
  const text = run(cm, args, options);
  try {
    return JSON.parse(text);
  } catch {
    throw new Error(`cm ${args.join(" ")} returned non-JSON:\n${text.slice(0, 2000)}`);
  }
}

function main() {
  let args;
  try {
    args = parseArgs(process.argv.slice(2));
  } catch (error) {
    console.error(error.message);
    usage();
    process.exit(2);
  }
  if (args.help) {
    usage();
    return;
  }
  for (const key of ["cm", "root", "out", "agentCmd", "reviewCmd"]) {
    if (!args[key]) {
      console.error(`missing --${key === "agentCmd" ? "agent-cmd" : key === "reviewCmd" ? "review-cmd" : key}`);
      usage();
      process.exit(2);
    }
  }
  const cm = resolve(args.cm);
  const root = resolve(args.root);
  const out = resolve(args.out);
  if (!existsSync(cm)) throw new Error(`cm not found: ${cm}`);
  if (!existsSync(root)) throw new Error(`root not found: ${root}`);

  console.error(`[wiki-auto] plan root=${root} out=${out}`);
  json(cm, ["wiki", "plan", "--root", root, "--out", out]);

  let lastCheck = { issues: ["not checked"] };
  for (let batch = 1; batch <= args.maxBatches; batch++) {
    const status = json(cm, ["wiki", "status", "--root", root, "--out", out]);
    const check = json(cm, ["wiki", "check", "--root", root, "--out", out, "--strict"], {
      allowFailure: true,
    });
    lastCheck = check;
    console.error(
      `[wiki-auto] batch ${batch}: written=${status.written} planned=${status.planned} stale=${status.stale} check_issues=${check.issues.length}`,
    );
    if (check.issues.length === 0) break;

    const generateArgs = [
      "wiki", "generate",
      "--root", root,
      "--out", out,
      "--agent-cmd", args.agentCmd,
      "--review-cmd", args.reviewCmd,
      "--review-rounds", String(args.rounds),
      "--max-pages", String(args.batch),
    ];
    if (args.task) generateArgs.push("--page", args.task);

    const report = json(cm, generateArgs, { timeout: args.timeout });
    console.error(
      `[wiki-auto] generated: attempted=${report.attempted} written=${report.written} failed=${report.failed.length}`,
    );
    if (report.failed.length) {
      for (const failure of report.failed.slice(0, 20)) console.error(`[wiki-auto] failed: ${failure}`);
    }
    if (report.attempted === 0) break;
  }

  const status = json(cm, ["wiki", "status", "--root", root, "--out", out]);
  lastCheck = json(cm, ["wiki", "check", "--root", root, "--out", out, "--strict"], {
    allowFailure: true,
  });
  const clean = lastCheck.issues.length === 0;
  console.error(
    `[wiki-auto] final: written=${status.written} planned=${status.planned} stale=${status.stale} review_pass=${status.review_passed} review_fail=${status.review_failed} clean=${clean}`,
  );

  let remember = null;
  if (args.remember || args.memory) {
    if (!args.memory) throw new Error("--remember requires --memory <file>");
    if (!clean) throw new Error("refusing wiki remember: strict check is not clean");
    remember = json(cm, ["wiki", "remember", "--file", args.memory, "--root", root, "--out", out]);
    console.error(`[wiki-auto] remember: ${remember.remembered}/${remember.pages}`);
  }

  console.log(
    JSON.stringify(
      {
        written: status.written,
        planned: status.planned,
        stale: status.stale,
        review_passed: status.review_passed,
        review_failed: status.review_failed,
        issues: lastCheck.issues.length,
        clean,
        remember,
      },
      null,
      2,
    ),
  );
  if (!clean) process.exit(1);
}

main();
