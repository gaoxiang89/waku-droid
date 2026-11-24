import { spawn, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

interface DaemonExposure {
  enabled?: boolean;
  port?: number;
  token?: string;
}

interface AppSettingsFile {
  daemon_exposure?: DaemonExposure;
}

interface BridgeStateFile {
  address?: string;
  token?: string;
  pid?: number;
}

interface DiscoveredDaemon {
  source: string;
  enabled: boolean | null;
  port: number;
  token: string | null;
}

interface CliOptions {
  command: 'status' | 'cloudflared' | 'help';
  portOverride: number | null;
  tokenOverride: string | null;
  showToken: boolean;
  extraArgs: string[];
}

const DEFAULT_EXPOSED_PORT = 34123;
const DEFAULT_SSH_DAEMON_PORT = 7420;

function printHelp(): void {
  console.log(`Waku Mobile Tunnel & V2Ray Connection Helper

Usage:
  bun run mobile:tunnel [status] [options]
  bun run mobile:tunnel cloudflared [options] [-- <extra cloudflared args>]

Commands:
  status        Inspect local daemon config and print connection templates (default)
  cloudflared   Launch an ephemeral Cloudflare Quick Tunnel (wss://*.trycloudflare.com)

Options:
  --port <port>   Override local daemon WebSocket port (default: auto-detected or ${DEFAULT_EXPOSED_PORT})
  --token <tok>   Override daemon authentication token
  --show-token    Print the full bearer token instead of masking it
  -h, --help      Show this help message
`);
}

function parsePort(raw: string): number {
  const port = Number(raw);
  if (!Number.isInteger(port) || port < 1 || port > 65535) {
    throw new Error(`Invalid port number: ${raw}`);
  }
  return port;
}

function parseArgs(argv: string[]): CliOptions {
  const doubleDashIndex = argv.indexOf('--');
  const mainArgs = doubleDashIndex === -1 ? argv : argv.slice(0, doubleDashIndex);
  const extraArgs = doubleDashIndex === -1 ? [] : argv.slice(doubleDashIndex + 1);

  let command: CliOptions['command'] = 'status';
  let portOverride: number | null = null;
  let tokenOverride: string | null = null;
  let showToken = false;

  for (let i = 0; i < mainArgs.length; i++) {
    const arg = mainArgs[i]!;
    if (arg === '-h' || arg === '--help' || arg === 'help') {
      command = 'help';
    } else if (arg === 'status' || arg === 'cloudflared') {
      command = arg;
    } else if (arg === '--show-token') {
      showToken = true;
    } else if (arg === '--port') {
      const next = mainArgs[++i];
      if (!next) throw new Error('--port requires a value');
      portOverride = parsePort(next);
    } else if (arg.startsWith('--port=')) {
      portOverride = parsePort(arg.slice('--port='.length));
    } else if (arg === '--token') {
      const next = mainArgs[++i];
      if (!next) throw new Error('--token requires a value');
      tokenOverride = next;
    } else if (arg.startsWith('--token=')) {
      tokenOverride = arg.slice('--token='.length);
    } else {
      throw new Error(`Unknown argument: ${arg}`);
    }
  }

  return { command, portOverride, tokenOverride, showToken, extraArgs };
}

function readJsonFile<T>(filePath: string): T | null {
  try {
    if (!fs.existsSync(filePath)) return null;
    const raw = fs.readFileSync(filePath, 'utf8');
    return JSON.parse(raw) as T;
  } catch {
    return null;
  }
}

function parsePortFromAddress(address: string | undefined): number | null {
  if (!address) return null;
  const match = address.match(/:(\d+)$/);
  if (!match) return null;
  const port = Number(match[1]);
  return Number.isInteger(port) && port > 0 && port <= 65535 ? port : null;
}

function discoverDaemons(repoRoot: string): DiscoveredDaemon[] {
  const home = os.homedir();
  const candidates: DiscoveredDaemon[] = [];

  const appPaths = [
    { label: 'Desktop release (~/.narel/app.json)', file: path.join(home, '.narel', 'app.json') },
    { label: 'Desktop debug (.waku-cache/app.json)', file: path.join(repoRoot, '.waku-cache', 'app.json') },
  ];

  for (const candidate of appPaths) {
    const data = readJsonFile<AppSettingsFile>(candidate.file);
    const exp = data?.daemon_exposure;
    if (!exp) continue;
    const port = typeof exp.port === 'number' ? exp.port : DEFAULT_EXPOSED_PORT;
    const token = typeof exp.token === 'string' && exp.token.trim() ? exp.token.trim() : null;
    candidates.push({
      source: candidate.label,
      enabled: Boolean(exp.enabled),
      port,
      token,
    });
  }

  const bridgePaths = [
    { label: 'Bridge daemon (~/.cache/narel/server/daemon.json)', file: path.join(home, '.cache', 'narel', 'server', 'daemon.json') },
    { label: 'Bridge dev daemon (~/.cache/narel-dev/server/daemon.json)', file: path.join(home, '.cache', 'narel-dev', 'server', 'daemon.json') },
  ];

  for (const candidate of bridgePaths) {
    const data = readJsonFile<BridgeStateFile>(candidate.file);
    if (!data) continue;
    const port = parsePortFromAddress(data.address);
    const token = typeof data.token === 'string' && data.token.trim() ? data.token.trim() : null;
    if (port) {
      candidates.push({
        source: candidate.label,
        enabled: true,
        port,
        token,
      });
    }
  }

  return candidates;
}

function selectPrimaryDaemon(discovered: DiscoveredDaemon[], options: CliOptions): DiscoveredDaemon {
  const preferred =
    discovered.find((item) => item.enabled === true) ??
    discovered[0] ?? {
      source: 'default (no local config file found)',
      enabled: null,
      port: DEFAULT_EXPOSED_PORT,
      token: null,
    };

  return {
    source: preferred.source,
    enabled: preferred.enabled,
    port: options.portOverride ?? preferred.port,
    token: options.tokenOverride ?? preferred.token,
  };
}

function formatToken(token: string | null, showToken: boolean): string {
  if (!token) return '<not set — copy from Waku Desktop -> Settings -> Daemon>';
  if (showToken) return token;
  if (token.length <= 8) return `${'*'.repeat(token.length)} (pass --show-token to reveal)`;
  return `${token.slice(0, 4)}...${token.slice(-4)} (pass --show-token to reveal)`;
}

function printStatus(discovered: DiscoveredDaemon[], primary: DiscoveredDaemon, showToken: boolean): void {
  console.log('=== Waku Mobile Connection Status ===');
  console.log(`Selected source : ${primary.source}`);
  console.log(`Target local URL: ws://127.0.0.1:${primary.port}`);
  console.log(`Bearer token    : ${formatToken(primary.token, showToken)}`);

  if (discovered.length > 1) {
    console.log('\nOther detected configs:');
    for (const item of discovered) {
      if (item === primary) continue;
      console.log(`  - ${item.source} (port=${item.port}, enabled=${item.enabled})`);
    }
  }

  if (primary.enabled === false) {
    console.log('\n[!] Desktop remote daemon exposure is currently DISABLED (`enabled: false`).');
    console.log('    Open Narel Desktop -> Settings -> Daemon and enable it so narel-daemon listens on 0.0.0.0:' + primary.port + '.');
  }

  const displayToken = primary.token && showToken ? primary.token : '<WAKU_DAEMON_TOKEN>';

  console.log(`
=== V2Ray-Compatible Android Connection Options ===
Note: Android allows only one OS VPN slot at a time, so keeping V2Ray active
prevents running the Tailscale app simultaneously. Also, Waku Mobile strips
URL pathnames (normalizeDaemonAddress), so always use a root host/subdomain
like wss://waku.example.com (never wss://example.com/waku).

1) Quickest Test — Cloudflare Quick Tunnel (zero VPS config):
   bun run mobile:tunnel cloudflared --port ${primary.port}
   Then enter the printed wss://<name>.trycloudflare.com URL + token in Waku Mobile.

   For a persistent subdomain with Cloudflare Named Tunnel:
     tunnel: <tunnel-uuid>
     ingress:
       - hostname: waku.example.com
         service: http://127.0.0.1:${primary.port}
       - service: http_status:404

2) Self-Hosted VPS (frp + Caddy -> wss://waku.example.com):
   # On your dev machine / SSH host (frpc.toml):
   serverAddr = "<VPS_PUBLIC_IP>"
   serverPort = 7000
   auth.token = "<FRP_SECRET>"

   [[proxies]]
   name = "narel-daemon"
   type = "tcp"
   localIP = "127.0.0.1"
   localPort = ${primary.port}
   remotePort = ${primary.port}

   # On your VPS (Caddyfile):
   waku.example.com {
     reverse_proxy 127.0.0.1:${primary.port}
   }

   Then in Narel Android:
     Address: wss://waku.example.com
     Token  : ${displayToken}
     (Optional: add DOMAIN-SUFFIX,waku.example.com,DIRECT in V2Ray for lower latency)

3) Share Live Sessions Between Desktop (SSH) and Android (WSS) on a Linux Host:
   Run narel-daemon with --state-file pointing to the bridge state file so both
   Narel Desktop ("narel-daemon bridge" over SSH) and Narel Android attach to the
   exact same daemon process:
     WAKU_DAEMON_TOKEN="${displayToken}" \\
       ~/.cache/narel/server/narel-daemon \\
       --bind 127.0.0.1:${DEFAULT_SSH_DAEMON_PORT} \\
       --state-file ~/.cache/narel/server/daemon.json
   Then tunnel 127.0.0.1:${DEFAULT_SSH_DAEMON_PORT} via frpc or cloudflared as above.

4) Alternative (Single VPN Slot via sing-box / Mihomo):
   If your Android proxy app runs sing-box (1.8+) or Mihomo, add Tailscale as
   an outbound endpoint and route 100.64.0.0/10 to it while keeping V2Ray as
   your default outbound. Then connect Waku Android to ws://<tailnet-ip>:${primary.port}.
`);
}

function runCloudflared(primary: DiscoveredDaemon, options: CliOptions): void {
  if (primary.enabled === false) {
    console.warn(`[warn] Desktop daemon_exposure.enabled is false in ${primary.source}.`);
    console.warn('       Enable it in Waku Desktop -> Settings -> Daemon if connections are refused.\n');
  }

  const probe = spawnSync('cloudflared', ['--version'], { encoding: 'utf8' });
  if (probe.error || probe.status !== 0) {
    console.error('Error: `cloudflared` was not found in PATH.');
    console.error('Install it first:');
    console.error('  Windows: winget install Cloudflare.cloudflared');
    console.error('  macOS  : brew install cloudflared');
    console.error('  Linux  : https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/');
    process.exit(1);
  }

  const targetUrl = `http://127.0.0.1:${primary.port}`;
  const args = ['tunnel', '--url', targetUrl, ...options.extraArgs];
  console.log(`Starting: cloudflared ${args.join(' ')}`);
  console.log('Waiting for trycloudflare.com URL...\n');

  const child = spawn('cloudflared', args, {
    stdio: ['inherit', 'pipe', 'pipe'],
  });

  let announced = false;
  const checkForUrl = (chunk: Buffer): void => {
    const text = chunk.toString('utf8');
    process.stderr.write(text);
    if (announced) return;
    const match = text.match(/https:\/\/([a-z0-9-]+\.trycloudflare\.com)/i);
    if (match?.[1]) {
      announced = true;
      const wssUrl = `wss://${match[1]}`;
      console.log('\n============================================================');
      console.log('Waku Android Connection Ready (V2Ray-compatible):');
      console.log(`  Address : ${wssUrl}`);
      console.log(`  Token   : ${formatToken(primary.token, options.showToken)}`);
      console.log('============================================================\n');
    }
  };

  child.stdout.on('data', (chunk: Buffer) => process.stdout.write(chunk));
  child.stderr.on('data', checkForUrl);

  const forwardSignal = (signal: NodeJS.Signals): void => {
    if (!child.killed) child.kill(signal);
  };
  process.on('SIGINT', forwardSignal);
  process.on('SIGTERM', forwardSignal);

  child.on('exit', (code) => {
    process.exit(code ?? 0);
  });
}

function main(): void {
  const options = parseArgs(process.argv.slice(2));
  if (options.command === 'help') {
    printHelp();
    return;
  }

  const repoRoot = path.resolve(import.meta.dirname ?? process.cwd(), '..');
  const discovered = discoverDaemons(repoRoot);
  const primary = selectPrimaryDaemon(discovered, options);

  if (options.command === 'cloudflared') {
    runCloudflared(primary, options);
    return;
  }

  printStatus(discovered, primary, options.showToken);
}

try {
  main();
} catch (error) {
  console.error(error instanceof Error ? error.message : String(error));
  process.exit(1);
}
