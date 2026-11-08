#!/usr/bin/env node
import { readFileSync } from "node:fs";
const prompt = readFileSync(0, "utf8");
const title = (prompt.match(/^PAGE: (.+)$/m) || [null, "Project Overview"])[1];
const page = `# ${title}

## Responsibility

This is a small demo repository used to exercise the automated wiki loop. The repository description is in \`README.md:1\`. The purpose is to verify that plan, generation, apply, mechanical checking, review, and the final strict gate work together without manual edits. The page is intentionally concise and every factual claim comes from the root readme.

## Public interfaces

The demo has no package manifest or exported API surface; the only target is the readme. A reader should treat this as documentation-only and not look for a runtime module. The readme itself is the interface for the demo. This section exists so the page has a clear public-boundary statement rather than a file inventory.

## Dependencies and tooling

There are no runtime dependencies, package scripts, or build tools in this demo. The only source of truth is the readme. A future version could add a package manifest, but this page must describe the current tree, not a hypothetical one. No UI or theme coupling exists.

## Failure modes

If the readme changes, the page becomes stale and must be regenerated. If the page omits the only target, the mechanical coverage check fails. If the reviewer rejects the page, the generator loop retries with the findings instead of writing a partial page. No automatic retry or compaction logic exists because this demo has no runtime.

## Configuration and tooling

The demo intentionally has no package scripts, dependency lockfile, compiler, or test runner. Configuration is limited to the readme itself, which documents the repository name and purpose. A reader should not infer build commands that the tree does not declare. The absence of a manifest is a deliberate constraint for the wiki loop test, not an oversight in the page.

## State and lifecycle

The wiki page has one lifecycle: planned, written after apply, reviewed by an independent reviewer, and accepted only when the strict check has no issues. The source hash ties the page to the readme; changing the readme invalidates the page. The manifest records the review verdict and source commit or content hash so a later session can tell whether the page is still trustworthy.

## Testing strategy

The test is the automated loop itself: a fake generator returns this page, a fake reviewer returns a pass verdict, and \`cm wiki check --strict\` verifies shape, citations, target coverage, and word budget. This exercises the plumbing without spending model tokens. A failed check leaves the page stale and the loop reports a non-zero exit code, which is the intended safety behavior.
`;
process.stdout.write(page);
