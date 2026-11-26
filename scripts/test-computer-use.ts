// Prohibition/lifecycle smoke test. No app discovery, screenshots, input, or
// TCC prompts.
// Usage: bun scripts/test-computer-use.ts <narel_js_repl> <computer-use-helper>
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { resolve, join } from "node:path";
import { createInterface } from "node:readline";

type ToolContent = { type: string; text?: string; data?: string };
type ToolResult = { isError?: boolean; content: ToolContent[] };
type JsonRpcResponse = { id?: number; result?: unknown; error?: { message?: string } };

function isJsonRpcResponse(value: unknown): value is JsonRpcResponse {
  return typeof value === "object" && value !== null && "id" in value;
}

function isToolResult(value: unknown): value is ToolResult {
  if (typeof value !== "object" || value === null || !("content" in value))
    return false;
  if (!Array.isArray(value.content)) return false;
  const items: unknown[] = value.content;
  return items.every(
    (item) => typeof item === "object" && item !== null && "type" in item,
  );
}

function toolText(result: ToolResult): string {
  const text = result.content[0]?.text;
  if (typeof text !== "string")
    throw new Error(`Missing tool text: ${JSON.stringify(result)}`);
  return text;
}

const suffix = process.platform === "win32" ? ".exe" : "";
const [
  repl = `target/debug/narel_js_repl${suffix}`,
  helper = `target/debug/narel_computer_use${suffix}`,
] = process.argv.slice(2);
assert(repl && helper, "Pass the REPL executable and native helper executable");
const directory = await mkdtemp(join(tmpdir(), "narel-cua-test-"));
const child = spawn(resolve(repl), [], {
  env: {
    ...process.env,
    // Point at a real helper so the refusal comes from the build policy, not
    // from a missing executable.
    WAKU_COMPUTER_USE_SERVER: resolve(helper),
    WAKU_COMPUTER_USE_PROCESS_DIRECTORY: directory,
  },
  stdio: ["pipe", "pipe", "pipe"],
  windowsHide: true,
});
let nextId = 0;
const pending = new Map<
  number,
  { resolve(value: unknown): void; reject(error: Error): void }
>();
createInterface({ input: child.stdout }).on("line", (line) => {
  const message: unknown = JSON.parse(line);
  if (!isJsonRpcResponse(message) || typeof message.id !== "number") return;
  const request = pending.get(message.id);
  if (!request) return;
  pending.delete(message.id);
  if (message.error)
    request.reject(new Error(message.error.message ?? "REPL request failed"));
  else request.resolve(message.result);
});
child.stderr.on("data", (chunk) => process.stderr.write(chunk));
child.on("error", (error) => {
  for (const request of pending.values()) request.reject(error);
});
child.on("exit", (code) => {
  for (const request of pending.values())
    request.reject(new Error(`REPL exited: ${code}`));
});
function request(method: string, params: object): Promise<unknown> {
  const id = ++nextId;
  const { promise, resolve, reject } = Promise.withResolvers<unknown>();
  const timer = setTimeout(() => {
    pending.delete(id);
    reject(new Error(`Timed out: ${method}`));
  }, 30_000);
  pending.set(id, {
    resolve: (value) => {
      clearTimeout(timer);
      resolve(value);
    },
    reject: (error) => {
      clearTimeout(timer);
      reject(error);
    },
  });
  child.stdin.write(
    JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n",
  );
  return promise;
}
async function js(code: string): Promise<ToolResult> {
  const result = await request("tools/call", {
    name: "js",
    arguments: { code },
  });
  assert.ok(isToolResult(result), `Unexpected tool result: ${JSON.stringify(result)}`);
  assert.notEqual(result.isError, true, JSON.stringify(result));
  return result;
}
// Every build prohibits Computer Use, so setup must reject even when the
// packaged helper is configured.
const attemptSetup = `
  var setupError = null;
  try { await setupComputerUseRuntime({ globals: globalThis }); }
  catch (error) { setupError = String((error && error.message) || error); }
  jsRepl.write(JSON.stringify({ setupError, hasSetup: typeof setupComputerUseRuntime, cuaAfter: typeof cua }));
`;
function assertRefused(result: ToolResult) {
  const output: unknown = JSON.parse(toolText(result));
  assert.ok(typeof output === "object" && output !== null);
  const { setupError, hasSetup, cuaAfter } = output as {
    setupError?: unknown;
    hasSetup?: unknown;
    cuaAfter?: unknown;
  };
  assert.equal(hasSetup, "function");
  assert.match(typeof setupError === "string" ? setupError : "", /not available/i);
  assert.equal(cuaAfter, "undefined");
}
function waitForExit(timeoutMs: number): Promise<void> {
  const { promise, resolve } = Promise.withResolvers<void>();
  const timer = setTimeout(resolve, timeoutMs);
  child.once("exit", () => {
    clearTimeout(timer);
    resolve();
  });
  return promise;
}
try {
  await request("initialize", {
    protocolVersion: "2025-06-18",
    capabilities: {},
    clientInfo: { name: "narel-cua-test", version: "1" },
  });
  const initial = await js("jsRepl.write(typeof cua)");
  assert.equal(toolText(initial), "undefined");
  assertRefused(await js(attemptSetup));
  const image = await js(
    'await jsRepl.emitImage({ type: "image", mimeType: "image/png", data: "cG5n" })',
  );
  assert(
    image.content.some(
      (item) => item.type === "image" && item.data === "cG5n",
    ),
  );
  await request("tools/call", { name: "js_reset", arguments: {} });
  assert.equal(toolText(await js("jsRepl.write(typeof cua)")), "undefined");
  assertRefused(await js(attemptSetup));
  console.log(
    `Cua prohibition smoke passed (${process.platform}): setup refused with WAKU_COMPUTER_USE_SERVER set; images and reset still work.`,
  );
} finally {
  child.stdin.end();
  await waitForExit(2_000);
  if (child.exitCode === null) child.kill();
  await rm(directory, { recursive: true, force: true });
}
