#!/usr/bin/env bun
//
// Build and package the Windows release: a portable zip and the Inno Setup
// installer the in-app updater re-runs silently. Mirrors bundle-linux.sh for
// the archive half and resources/windows/waku.iss for the installer half.
//
// Usage:
//   bun scripts/bundle-windows.ts
//
// Env:
//   CARGO_TARGET_DIR              cargo target directory (default: target)
//   WINDOWS_CERTIFICATE           base64 Authenticode .pfx (optional)
//   WINDOWS_CERTIFICATE_PASSWORD  password for it
//   WAKU_REMOTE_DAEMONS_DIR       directory with waku-daemon-linux-{x86_64,aarch64}
//                                 for WSL and SSH hosts (release CI sets it)
//   WAKU_REQUIRE_REMOTE_DAEMONS   set to "1" to fail if Linux daemons are missing
import { $ } from "bun";
import { bundleComputerUse } from "./cua-driver";
import { existsSync, readdirSync, statSync } from "node:fs";
import { copyFile, mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const packageName = "waku";
const projectRoot = resolve(import.meta.dir, "..");

/** The updater picks its feed by Rust arch name, so the installer carries
 *  that rather than the full triple. */
const architectureForTarget: Record<string, string> = {
  "x86_64-pc-windows-msvc": "x86_64",
  "aarch64-pc-windows-msvc": "aarch64",
};

interface CargoMetadata {
  packages: { name: string; version: string }[];
}

/** Inno Setup's compiler, however it was installed: on `PATH`, per-machine
 *  under Program Files, or per-user under `%LOCALAPPDATA%\Programs`, which is
 *  where `winget install JRSoftware.InnoSetup` puts it without elevation. */
function findInnoSetupCompiler(): string {
  const onPath = Bun.which("ISCC.exe") ?? Bun.which("iscc");
  if (onPath) return onPath;
  const localPrograms = process.env.LOCALAPPDATA
    ? join(process.env.LOCALAPPDATA, "Programs")
    : undefined;
  for (const base of [
    process.env.ProgramFiles,
    process.env["ProgramFiles(x86)"],
    localPrograms,
  ]) {
    if (!base) continue;
    const candidate = join(base, "Inno Setup 6", "ISCC.exe");
    if (existsSync(candidate)) return candidate;
  }
  throw new Error(
    "ISCC.exe was not found. Install Inno Setup 6: " +
      "`winget install --id JRSoftware.InnoSetup` or `choco install innosetup`.",
  );
}

/** The newest signtool in the installed Windows SDKs, preferring the host's
 *  own architecture. An SDK lays these out as `bin\<version>\<arch>`, with
 *  older ones dropping the version directory. */
function findSigntool(): string {
  const root = join(
    process.env["ProgramFiles(x86)"] ?? "C:\\Program Files (x86)",
    "Windows Kits",
    "10",
    "bin",
  );
  if (!existsSync(root)) {
    throw new Error(`signtool.exe was not found: ${root} does not exist.`);
  }
  const versionOrder = new Intl.Collator("en", { numeric: true });
  const versions = readdirSync(root).sort((a, b) => versionOrder.compare(b, a));
  // arm64 Windows runs the x64 tool under emulation, so it stays as a
  // fallback rather than a failure.
  const architectures =
    process.arch === "arm64" ? ["arm64", "x64", "x86"] : ["x64", "x86"];
  for (const architecture of architectures) {
    for (const directory of [...versions.map((v) => join(root, v)), root]) {
      const candidate = join(directory, architecture, "signtool.exe");
      if (existsSync(candidate)) return candidate;
    }
  }
  throw new Error(`signtool.exe was not found under ${root}.`);
}

/** Linux daemons for WSL and SSH hosts, which this Windows build cannot
 *  compile. Release CI stages both and sets WAKU_REQUIRE_REMOTE_DAEMONS=1 to
 *  enforce them; a local package usually carries only the one its WSL
 *  distribution needs, so missing architectures warn unless required. */
const expectedLinuxDaemons = [
  "waku-daemon-linux-x86_64",
  "waku-daemon-linux-aarch64",
];

interface StagedRemoteDaemons {
  windowsExecutables: string[];
  hasRemoteDaemons: boolean;
}

async function stageRemoteDaemons(
  destination: string,
): Promise<StagedRemoteDaemons> {
  const requireRemoteDaemons = process.env.WAKU_REQUIRE_REMOTE_DAEMONS === "1";
  const source = process.env.WAKU_REMOTE_DAEMONS_DIR;
  if (!source) {
    const message =
      "WAKU_REMOTE_DAEMONS_DIR is not set; this package cannot connect to WSL or Linux SSH hosts.";
    if (requireRemoteDaemons) {
      throw new Error(message);
    }
    console.warn(message);
    return { windowsExecutables: [], hasRemoteDaemons: false };
  }
  const directory = resolve(source);
  if (!existsSync(directory) || !statSync(directory).isDirectory()) {
    throw new Error(`WAKU_REMOTE_DAEMONS_DIR does not exist: ${directory}`);
  }
  const stagedWindowsExecutables: string[] = [];
  let stagedCount = 0;
  for (const name of readdirSync(directory).sort()) {
    if (!/^waku-daemon-[A-Za-z0-9_.-]+$/.test(name)) continue;
    const file = join(directory, name);
    if (!statSync(file).isFile()) continue;
    if (stagedCount === 0) {
      await mkdir(destination, { recursive: true });
    }
    const target = join(destination, name);
    await copyFile(file, target);
    stagedCount += 1;
    if (name.toLowerCase().endsWith(".exe")) {
      stagedWindowsExecutables.push(target);
    }
  }
  const invalid = expectedLinuxDaemons.flatMap((name) => {
    const target = join(destination, name);
    if (!existsSync(target)) return [`${name} (missing)`];
    const stats = statSync(target);
    if (!stats.isFile() || stats.size === 0) return [`${name} (empty)`];
    return [];
  });
  if (invalid.length > 0) {
    const message =
      `WAKU_REMOTE_DAEMONS_DIR has missing or empty ${invalid.join(", ")}; ` +
      "this package cannot connect to those Linux hosts.";
    if (requireRemoteDaemons) {
      throw new Error(message);
    }
    console.warn(message);
  }
  return {
    windowsExecutables: stagedWindowsExecutables,
    hasRemoteDaemons: stagedCount > 0,
  };
}

async function sign(
  signtool: string,
  certificate: string,
  password: string,
  files: string[],
): Promise<void> {
  for (const file of files) {
    await $`${signtool} sign /f ${certificate} /p ${password} /fd SHA256 /tr http://timestamp.digicert.com /td SHA256 ${file}`;
  }
}

process.chdir(projectRoot);

if (process.platform !== "win32") {
  throw new Error(
    "bundle-windows.ts builds a native Windows release and must run on Windows.",
  );
}

const targetDirectory = resolve(process.env.CARGO_TARGET_DIR || "target");
const releaseDirectory = join(targetDirectory, "release");

const metadata = JSON.parse(
  await $`cargo metadata --no-deps --format-version 1`.quiet().text(),
) as CargoMetadata;
const version = metadata.packages.find(
  (candidate) => candidate.name === packageName,
)?.version;
if (!version) {
  throw new Error(`Cargo package "${packageName}" was not found.`);
}

const hostLine = (await $`rustc -vV`.quiet().text())
  .split("\n")
  .find((line) => line.startsWith("host: "));
const targetTriple = hostLine?.slice("host: ".length).trim();
const architecture = targetTriple
  ? architectureForTarget[targetTriple]
  : undefined;
if (!targetTriple || !architecture) {
  throw new Error(`Unsupported Windows target ${targetTriple ?? "(unknown)"}`);
}

const packageDirectoryName = `waku-${version}-${targetTriple}`;
const archive = join(releaseDirectory, `${packageDirectoryName}.zip`);
const installer = join(
  releaseDirectory,
  `Waku-${version}-${architecture}-Setup.exe`,
);

await $`cargo build --locked --release --package waku --bin waku --bin waku_js_repl --package waku-daemon --bin waku-daemon --package waku-computer-use --bin waku_computer_use`;

const staging = await mkdtemp(join(tmpdir(), "waku-bundle-"));
try {
  // Both executables stay side by side: the app resolves the daemon next to
  // itself, so the layout is what makes an extracted zip runnable in place.
  const packageDirectory = join(staging, packageDirectoryName);
  await mkdir(packageDirectory, { recursive: true });
  await bundleComputerUse(
    packageDirectory,
    join(packageDirectory, "resources"),
    "release",
  );
  for (const file of ["waku.exe", "waku-daemon.exe"]) {
    await copyFile(join(releaseDirectory, file), join(packageDirectory, file));
  }
  await copyFile(
    join(projectRoot, "LICENSE"),
    join(packageDirectory, "LICENSE"),
  );
  const {
    windowsExecutables: stagedRemoteWindowsDaemons,
    hasRemoteDaemons,
  } = await stageRemoteDaemons(join(packageDirectory, "remote"));

  // Authenticode has to be applied before anything is packaged, so the
  // executables inside the zip and the installer are all signed. Unsigned
  // builds still package, so a fork without a certificate can release.
  const certificateData = process.env.WINDOWS_CERTIFICATE;
  const certificatePassword = process.env.WINDOWS_CERTIFICATE_PASSWORD;
  let certificate: string | undefined;
  let signtool: string | undefined;
  if (certificateData && certificatePassword) {
    certificate = join(staging, "certificate.pfx");
    await writeFile(certificate, Buffer.from(certificateData, "base64"));
    signtool = findSigntool();
    await sign(signtool, certificate, certificatePassword, [
      join(packageDirectory, "waku.exe"),
      join(packageDirectory, "waku-daemon.exe"),
      join(packageDirectory, "waku_js_repl.exe"),
      join(packageDirectory, "waku_computer_use.exe"),
      join(packageDirectory, "cua_driver_sdk.dll"),
      join(packageDirectory, "cua-driver-uia.exe"),
      ...stagedRemoteWindowsDaemons,
    ]);
  } else {
    console.log("No WINDOWS_CERTIFICATE set; packaging unsigned binaries.");
  }

  await mkdir(releaseDirectory, { recursive: true });
  await rm(archive, { force: true });
  // Windows 10 1803 and later ship bsdtar, which writes a zip when the output
  // name says so — no PowerShell, and the same one-versioned-directory layout
  // the Linux tarball uses.
  await $`tar -a -c -f ${archive} -C ${staging} ${packageDirectoryName}`;
  console.log(`Created ${archive}`);

  // The installer is what the in-app updater downloads and re-runs, so it
  // ships from the same signed staging directory as the zip.
  await rm(installer, { force: true });
  await $`${findInnoSetupCompiler()} ${`/DAppVersion=${version}`} ${`/DArch=${architecture}`} ${`/DStageDir=${packageDirectory}`} ${`/DOutputDir=${releaseDirectory}`} ${`/DHasRemoteDaemons=${hasRemoteDaemons ? 1 : 0}`} ${join(projectRoot, "resources", "windows", "waku.iss")}`;
  if (!existsSync(installer)) {
    throw new Error(`ISCC did not produce ${installer}`);
  }
  if (certificate && signtool && certificatePassword) {
    await sign(signtool, certificate, certificatePassword, [installer]);
  }
  console.log(`Created ${installer} (${statSync(installer).size} bytes)`);
} finally {
  await rm(staging, { recursive: true, force: true });
}
