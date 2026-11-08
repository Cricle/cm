#!/usr/bin/env node
import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";

const prompt = readFileSync(0, "utf8");
if (!prompt.trim()) {
  console.error("zcode_agent: empty prompt on stdin");
  process.exit(2);
}
const zcode = "C:/wp/zcode/resources/glm/zcode.cjs";
const result = spawnSync(process.execPath, [zcode, `--prompt=${prompt}`, "--cwd", process.cwd(), "--mode", "plan"], {
  encoding: "utf8",
  maxBuffer: 128 * 1024 * 1024,
  env: process.env,
});
if (result.stdout) process.stdout.write(result.stdout);
if (result.stderr) process.stderr.write(result.stderr);
process.exit(result.status ?? 1);
