#!/usr/bin/env node
import fs from "node:fs";
import path from "node:path";
import readline from "node:readline";
import { spawn } from "node:child_process";

const command = process.env.AION_MCP_AUDIT_COMMAND;
const args = JSON.parse(process.env.AION_MCP_AUDIT_ARGS_JSON || "[]");
const envOverlay = JSON.parse(process.env.AION_MCP_AUDIT_ENV_JSON || "{}");
const auditPath = process.env.AION_MCP_AUDIT_LOG;
const serverName = process.env.AION_MCP_AUDIT_SERVER_NAME || "mcp";

if (!command || !auditPath) {
  console.error("missing AION_MCP_AUDIT_COMMAND or AION_MCP_AUDIT_LOG");
  process.exit(2);
}

fs.mkdirSync(path.dirname(auditPath), { recursive: true });

const child = spawn(command, args, {
  stdio: ["pipe", "pipe", "pipe"],
  env: { ...process.env, ...envOverlay },
});

const pending = new Map();

function append(record) {
  try {
    fs.appendFileSync(auditPath, `${JSON.stringify({ ts: Date.now(), server_name: serverName, ...record })}\n`);
  } catch {
    // Never corrupt MCP stdio because audit persistence failed.
  }
}

child.stderr.on("data", (chunk) => process.stderr.write(chunk));
child.on("exit", (code, signal) => {
  append({ event: "exit", code, signal });
  process.exit(code ?? (signal ? 1 : 0));
});

readline.createInterface({ input: process.stdin }).on("line", (line) => {
  try {
    const msg = JSON.parse(line);
    if (msg?.method === "tools/call" && msg?.id != null) {
      pending.set(String(msg.id), {
        tool_name: msg.params?.name || null,
        arguments: msg.params?.arguments ?? null,
      });
      append({
        event: "tools_call_request",
        jsonrpc_id: msg.id,
        tool_name: msg.params?.name || null,
        arguments: msg.params?.arguments ?? null,
      });
    }
  } catch {}
  child.stdin.write(`${line}\n`);
});

process.stdin.on("end", () => child.stdin.end());

readline.createInterface({ input: child.stdout }).on("line", (line) => {
  try {
    const msg = JSON.parse(line);
    if (msg?.id != null) {
      const pendingCall = pending.get(String(msg.id));
      if (pendingCall) {
        pending.delete(String(msg.id));
        append({
          event: "tools_call_response",
          jsonrpc_id: msg.id,
          tool_name: pendingCall.tool_name,
          arguments: pendingCall.arguments,
          result: msg.result ?? null,
          error: msg.error ?? null,
        });
      }
    }
  } catch {}
  process.stdout.write(`${line}\n`);
});
