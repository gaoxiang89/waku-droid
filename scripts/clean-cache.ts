#!/usr/bin/env bun

// Reclaim the build caches that grow without bound.
//
// Incremental compilation is off in .cargo/config.toml, but a checkout that
// predates that switch still holds every session directory rustc ever wrote,
// and the Cua SDK's release target is only needed while the SDK is being
// built. Both are safe to delete: the next build recreates them, and
// `target/debug/deps` (the expensive part) is deliberately left alone.
//
//   bun run clean:cache -- --dry-run       show what would be removed
//   bun run clean:cache                    prompt, then remove
//   bun run clean:cache -- --yes           remove without prompting
//   bun run clean:cache -- --yes --force   also when a build is running

import { existsSync } from "node:fs";
import { lstat, readdir, rm } from "node:fs/promises";
import { join, relative, resolve } from "node:path";
import { createInterface } from "node:readline/promises";

const projectRoot = resolve(import.meta.dir, "..");
const targetRoot = resolve(
  projectRoot,
  process.env.CARGO_TARGET_DIR || "target",
);
const cuaHostTarget = join(projectRoot, ".waku-cache", "cua-host", "target");

const argv = new Set(process.argv.slice(2));
const dryRun = argv.has("--dry-run") || argv.has("-n");
const assumeYes = argv.has("--yes") || argv.has("-y");
const force = argv.has("--force");

type Candidate = { path: string; bytes: number; files: number };

function usage(): void {
  console.log(`Usage: bun run clean:cache -- [options]

Removes incremental session directories under ${relative(projectRoot, targetRoot) || "."}/
and the Cua SDK release target in .waku-cache/cua-host/target.

Options:
  -n, --dry-run  List what would be deleted, delete nothing
  -y, --yes      Skip the confirmation prompt (required without a TTY)
      --force    Proceed even if a Cargo build is running`);
}

if (argv.has("--help") || argv.has("-h")) {
  usage();
  process.exit(0);
}

for (const argument of argv) {
  if (!["--dry-run", "-n", "--yes", "-y", "--force"].includes(argument)) {
    console.error(`Unknown argument: ${argument}\n`);
    usage();
    process.exit(1);
  }
}

function human(bytes: number): string {
  if (bytes >= 1024 ** 3) return `${(bytes / 1024 ** 3).toFixed(1)} GB`;
  if (bytes >= 1024 ** 2) return `${(bytes / 1024 ** 2).toFixed(1)} MB`;
  if (bytes >= 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${bytes} B`;
}

async function measure(path: string): Promise<{ bytes: number; files: number }> {
  let bytes = 0;
  let files = 0;
  const pending = [path];
  while (pending.length > 0) {
    const current = pending.pop()!;
    let entries;
    try {
      entries = await readdir(current, { withFileTypes: true });
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code === "ENOENT") continue;
      throw error;
    }
    const sizes = await Promise.all(
      entries.map(async (entry) => {
        const full = join(current, entry.name);
        if (entry.isDirectory()) {
          pending.push(full);
          return 0;
        }
        if (!entry.isFile()) return 0;
        try {
          const metadata = await lstat(full);
          files += 1;
          return metadata.size;
        } catch {
          return 0;
        }
      }),
    );
    bytes += sizes.reduce((total, size) => total + size, 0);
  }
  return { bytes, files };
}

// Cargo puts one `incremental` directory in each profile directory, both at
// the target root (target/debug) and under a cross-compilation triple
// (target/<triple>/debug).
async function incrementalDirectories(): Promise<string[]> {
  const found: string[] = [];
  let entries;
  try {
    entries = await readdir(targetRoot, { withFileTypes: true });
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return found;
    throw error;
  }
  for (const entry of entries) {
    if (!entry.isDirectory()) continue;
    const base = join(targetRoot, entry.name);
    const direct = join(base, "incremental");
    if (existsSync(direct)) found.push(direct);
    for (const profile of ["debug", "release"]) {
      const nested = join(base, profile, "incremental");
      if (existsSync(nested)) found.push(nested);
    }
  }
  return found;
}

function commandLines(): string[] {
  try {
    const output =
      process.platform === "win32"
        ? Bun.spawnSync(
            [
              "powershell",
              "-NoProfile",
              "-Command",
              'Get-CimInstance Win32_Process | ForEach-Object { "$($_.Name) $($_.CommandLine)" }',
            ],
            { stdout: "pipe", stderr: "ignore" },
          ).stdout.toString()
        : Bun.spawnSync(["ps", "-Ao", "args="], {
            stdout: "pipe",
            stderr: "ignore",
          }).stdout.toString();
    return output.split(/\r?\n/).filter((line) => line.trim() !== "");
  } catch {
    return [];
  }
}

function executableName(line: string): string {
  const first = line.trim().split(/\s+/)[0] ?? "";
  const bare = first.replace(/^"|"$/g, "");
  const base = bare.split(/[\\/]/).pop() ?? "";
  return base.replace(/\.exe$/i, "").toLowerCase();
}

const lines = commandLines();
const builders = lines.filter((line) =>
  ["cargo", "rustc", "rustup"].includes(executableName(line)),
);
const watchers = lines.filter((line) => /scripts[\\/]dev\.ts\b/.test(line));

if (builders.length > 0 && !force) {
  console.error(
    "A Cargo build is running, so deleting these caches would corrupt it:",
  );
  for (const line of builders.slice(0, 3)) {
    console.error(`  ${line.trim().slice(0, 160)}`);
  }
  console.error("\nWait for the build to finish, or pass --force.");
  process.exit(3);
}

const candidates: Candidate[] = [];
for (const path of await incrementalDirectories()) {
  const { bytes, files } = await measure(path);
  if (files > 0) candidates.push({ path, bytes, files });
}
if (existsSync(cuaHostTarget)) {
  const { bytes, files } = await measure(cuaHostTarget);
  if (files > 0) candidates.push({ path: cuaHostTarget, bytes, files });
}

if (candidates.length === 0) {
  console.log("Nothing to clean.");
  process.exit(0);
}

const totalBytes = candidates.reduce((sum, item) => sum + item.bytes, 0);
const totalFiles = candidates.reduce((sum, item) => sum + item.files, 0);

console.log("Build caches to remove:\n");
for (const candidate of candidates) {
  const label = relative(projectRoot, candidate.path) || candidate.path;
  console.log(
    `  ${human(candidate.bytes).padStart(9)}  ${candidate.files
      .toLocaleString("en-US")
      .padStart(9)} files  ${label}`,
  );
}
console.log(`\nTotal: ${human(totalBytes)} (${totalFiles.toLocaleString("en-US")} files)`);

if (watchers.length > 0) {
  console.warn(
    "\nNote: the dev watcher (scripts/dev.ts) is running; its next rebuild will be a full one.",
  );
}

if (dryRun) {
  console.log("\nDry run: nothing was deleted.");
  process.exit(0);
}

if (!assumeYes) {
  if (!process.stdin.isTTY) {
    console.error(
      "\nRefusing to delete without a TTY confirmation. Pass --yes or --dry-run.",
    );
    process.exit(2);
  }
  const readline = createInterface({
    input: process.stdin,
    output: process.stdout,
  });
  let answer: string;
  try {
    answer = (await readline.question("\nDelete these caches? [y/N] "))
      .trim()
      .toLowerCase();
  } finally {
    readline.close();
  }
  if (answer !== "y" && answer !== "yes") {
    console.log("Cancelled; nothing was deleted.");
    process.exit(0);
  }
}

const failures: Array<{ path: string; error: unknown }> = [];
let freed = 0;
for (const candidate of candidates) {
  const label = relative(projectRoot, candidate.path) || candidate.path;
  try {
    await rm(candidate.path, { recursive: true, force: true });
    freed += candidate.bytes;
    console.log(`Deleted ${label} (${human(candidate.bytes)})`);
  } catch (error) {
    failures.push({ path: candidate.path, error });
    console.error(`Could not delete ${label}:`, error);
  }
}

if (failures.length > 0) {
  console.error(
    `\nDeleted ${candidates.length - failures.length} of ${candidates.length} caches (${human(freed)} freed); ${failures.length} failed.`,
  );
  process.exit(1);
}

console.log(`\nFreed ${human(freed)}.`);
