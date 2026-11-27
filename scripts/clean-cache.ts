#!/usr/bin/env bun

// Reclaim the build caches that grow without bound.
//
// Incremental compilation is enabled only for the hand-edited workspace crates
// (see `[profile.dev.package.*]` in Cargo.toml). rustc keeps several sessions
// per crate and never prunes the directories left by an older profile or
// metadata hash, so a long-running checkout still accumulates gigabytes. The
// prune below keeps the newest sessions of each crate cache, which are the ones
// the next build reuses, and drops everything else; the Cua SDK's release target
// is only needed while the SDK is being built. `target/debug/deps` (the
// expensive part) is deliberately left alone.
//
//   bun run clean:cache -- --dry-run       show what would be removed
//   bun run clean:cache                    prompt, then prune
//   bun run clean:cache -- --yes           prune without prompting
//   bun run clean:cache -- --yes --all     remove every incremental cache
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

/** rustc names incremental session directories `s-<session>-<timestamp>-...`. */
const SESSION_PREFIX = "s-";

export function humanBytes(bytes: number): string {
  if (bytes >= 1024 ** 3) return `${(bytes / 1024 ** 3).toFixed(1)} GB`;
  if (bytes >= 1024 ** 2) return `${(bytes / 1024 ** 2).toFixed(1)} MB`;
  if (bytes >= 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${bytes} B`;
}

export type PruneOptions = {
  /** Sessions kept per crate cache, newest first. */
  keep?: number;
  /** Remove a whole crate cache untouched for this long. */
  maxAgeDays?: number;
  /** Report what would be removed without deleting. */
  dryRun?: boolean;
};

export type PruneResult = {
  bytes: number;
  files: number;
  /** Deleted (or, for a dry run, deletable) cache paths, relative to `target`. */
  removed: string[];
};

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

type Session = { path: string; name: string; mtimeMs: number };

/**
 * Bound the incremental caches to their newest sessions.
 *
 * Safe to call right after a build finishes: the newest sessions are the ones
 * rustc reuses next, and they are never removed. Only the extra history and
 * caches left behind by a superseded profile or metadata hash go away.
 */
export async function pruneIncrementalSessions(
  options: PruneOptions = {},
): Promise<PruneResult> {
  const keep = Math.max(1, Math.trunc(options.keep ?? 2));
  const maxAgeMs =
    (options.maxAgeDays ?? 7) * 24 * 60 * 60 * 1000;
  const now = Date.now();
  const result: PruneResult = { bytes: 0, files: 0, removed: [] };

  for (const root of await incrementalDirectories()) {
    let crateEntries;
    try {
      crateEntries = await readdir(root, { withFileTypes: true });
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code === "ENOENT") continue;
      throw error;
    }

    for (const crateEntry of crateEntries) {
      if (!crateEntry.isDirectory()) continue;
      const cratePath = join(root, crateEntry.name);
      let children;
      try {
        children = await readdir(cratePath, { withFileTypes: true });
      } catch {
        continue;
      }

      const sessions: Session[] = [];
      for (const child of children) {
        if (!child.isDirectory() || !child.name.startsWith(SESSION_PREFIX)) {
          continue;
        }
        const path = join(cratePath, child.name);
        try {
          const metadata = await lstat(path);
          sessions.push({ path, name: child.name, mtimeMs: metadata.mtimeMs });
        } catch {
          // A session removed by a concurrent rustc is already gone.
        }
      }
      if (sessions.length === 0) continue;
      sessions.sort((a, b) => b.mtimeMs - a.mtimeMs);

      // A crate cache untouched for a week belongs to a profile or metadata
      // hash nothing builds any more; keeping its newest session only leaks
      // disk.
      const stale = now - sessions[0].mtimeMs > maxAgeMs;
      const doomed = stale ? sessions : sessions.slice(keep);
      if (doomed.length === 0) continue;

      const doomedNames = new Set(doomed.map((session) => session.name));
      for (const session of doomed) {
        const { bytes, files } = await measure(session.path);
        result.bytes += bytes;
        result.files += files;
        result.removed.push(relative(targetRoot, session.path));
        if (!options.dryRun) {
          await rm(session.path, { recursive: true, force: true });
        }
      }

      // rustc writes one `s-<session>-<timestamp>.lock` beside each session
      // directory; drop the ones whose session just went away.
      const keptNames = sessions
        .filter((session) => !doomedNames.has(session.name))
        .map((session) => session.name);
      for (const child of children) {
        if (!child.name.endsWith(".lock")) continue;
        const lockBase = child.name.slice(0, -".lock".length);
        if (keptNames.some((name) => name.startsWith(lockBase))) continue;
        if (!options.dryRun) {
          await rm(join(cratePath, child.name), { force: true });
        }
      }

      if (stale) {
        // The session loop above already emptied this directory; remove the
        // now-empty crate cache as well.
        if (!options.dryRun) {
          try {
            await rm(cratePath, { recursive: true, force: true });
          } catch {
            // Best effort; an empty directory costs nothing.
          }
        }
      }
    }
  }

  return result;
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

function buildersRunning(): string[] {
  return commandLines().filter((line) =>
    ["cargo", "rustc", "rustup"].includes(executableName(line)),
  );
}

function watchersRunning(): string[] {
  return commandLines().filter((line) => /scripts[\\/]dev\.ts\b/.test(line));
}

function usage(): void {
  console.log(`Usage: bun run clean:cache -- [options]

Prunes the incremental caches under ${relative(projectRoot, targetRoot) || "."}/
to their two newest sessions and removes the Cua SDK release target in
.waku-cache/cua-host/target.

Options:
  -n, --dry-run        List what would be deleted, delete nothing
  -y, --yes            Skip the confirmation prompt (required without a TTY)
      --all            Remove every incremental cache, not just old sessions
      --keep <n>       Sessions kept per crate cache (default: 2)
      --max-age-days <n>
                       Remove crate caches untouched for n days (default: 7)
      --force          Proceed even if a Cargo build is running`);
}

async function main(): Promise<void> {
  const argv = process.argv.slice(2);
  let dryRun = false;
  let assumeYes = false;
  let force = false;
  let removeAll = false;
  let keep = 2;
  let maxAgeDays = 7;

  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    switch (argument) {
      case "-n":
      case "--dry-run":
        dryRun = true;
        break;
      case "-y":
      case "--yes":
        assumeYes = true;
        break;
      case "--force":
        force = true;
        break;
      case "--all":
        removeAll = true;
        break;
      case "--keep": {
        const value = Number.parseInt(argv[index + 1] ?? "", 10);
        if (!Number.isInteger(value) || value < 1) {
          console.error("--keep needs a positive integer.\n");
          usage();
          process.exit(1);
        }
        keep = value;
        index += 1;
        break;
      }
      case "--max-age-days": {
        const value = Number.parseInt(argv[index + 1] ?? "", 10);
        if (!Number.isInteger(value) || value < 0) {
          console.error("--max-age-days needs a non-negative integer.\n");
          usage();
          process.exit(1);
        }
        maxAgeDays = value;
        index += 1;
        break;
      }
      case "--help":
      case "-h":
        usage();
        process.exit(0);
      default:
        console.error(`Unknown argument: ${argument}\n`);
        usage();
        process.exit(1);
    }
  }

  const builders = buildersRunning();
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

  type Candidate = { path: string; bytes: number; files: number };

  const candidates: Candidate[] = [];

  if (existsSync(cuaHostTarget)) {
    const { bytes, files } = await measure(cuaHostTarget);
    if (files > 0) candidates.push({ path: cuaHostTarget, bytes, files });
  }

  if (removeAll) {
    for (const path of await incrementalDirectories()) {
      const { bytes, files } = await measure(path);
      if (files > 0) candidates.push({ path, bytes, files });
    }
  }

  if (!removeAll) {
    const pruned = await pruneIncrementalSessions({
      keep,
      maxAgeDays,
      dryRun: true,
    });
    if (pruned.files === 0 && candidates.length === 0) {
      console.log("Nothing to clean.");
      process.exit(0);
    }

    console.log("Build caches to remove:\n");
    for (const removed of pruned.removed) {
      console.log(`  ${removed}`);
    }
    for (const candidate of candidates) {
      const label = relative(projectRoot, candidate.path) || candidate.path;
      console.log(
        `  ${humanBytes(candidate.bytes).padStart(9)}  ${candidate.files
          .toLocaleString("en-US")
          .padStart(9)} files  ${label}`,
      );
    }
    console.log(
      `\nIncremental sessions: ${pruned.removed.length} (${humanBytes(
        pruned.bytes,
      )}, ${pruned.files.toLocaleString("en-US")} files)`,
    );

    const watchers = watchersRunning();
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
      await confirmDeletion();
    }

    const prunedNow = await pruneIncrementalSessions({ keep, maxAgeDays });
    let freed = prunedNow.bytes;
    console.log(
      `Pruned ${prunedNow.removed.length} incremental session(s) (${humanBytes(
        prunedNow.bytes,
      )}).`,
    );
    for (const candidate of candidates) {
      const label = relative(projectRoot, candidate.path) || candidate.path;
      try {
        await rm(candidate.path, { recursive: true, force: true });
        freed += candidate.bytes;
        console.log(`Deleted ${label} (${humanBytes(candidate.bytes)})`);
      } catch (error) {
        console.error(`Could not delete ${label}:`, error);
        process.exitCode = 1;
      }
    }
    console.log(`\nFreed ${humanBytes(freed)}.`);
    return;
  }

  if (candidates.length === 0) {
    console.log("Nothing to clean.");
    return;
  }

  const totalBytes = candidates.reduce((sum, item) => sum + item.bytes, 0);
  const totalFiles = candidates.reduce((sum, item) => sum + item.files, 0);

  console.log("Build caches to remove:\n");
  for (const candidate of candidates) {
    const label = relative(projectRoot, candidate.path) || candidate.path;
    console.log(
      `  ${humanBytes(candidate.bytes).padStart(9)}  ${candidate.files
        .toLocaleString("en-US")
        .padStart(9)} files  ${label}`,
    );
  }
  console.log(
    `\nTotal: ${humanBytes(totalBytes)} (${totalFiles.toLocaleString("en-US")} files)`,
  );

  const watchers = watchersRunning();
  if (watchers.length > 0) {
    console.warn(
      "\nNote: the dev watcher (scripts/dev.ts) is running; its next rebuild will be a full one.",
    );
  }

  if (dryRun) {
    console.log("\nDry run: nothing was deleted.");
    return;
  }
  if (!assumeYes) {
    await confirmDeletion();
  }

  let freed = 0;
  for (const candidate of candidates) {
    const label = relative(projectRoot, candidate.path) || candidate.path;
    try {
      await rm(candidate.path, { recursive: true, force: true });
      freed += candidate.bytes;
      console.log(`Deleted ${label} (${humanBytes(candidate.bytes)})`);
    } catch (error) {
      console.error(`Could not delete ${label}:`, error);
      process.exitCode = 1;
    }
  }
  console.log(`\nFreed ${humanBytes(freed)}.`);
}

async function confirmDeletion(): Promise<void> {
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

if (import.meta.main) {
  await main();
}
