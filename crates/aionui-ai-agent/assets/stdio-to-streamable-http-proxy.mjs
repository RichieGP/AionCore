#!/usr/bin/env node

import http from "node:http";
import process from "node:process";
import { randomUUID } from "node:crypto";
import { pathToFileURL } from "node:url";

const nodeModules =
  process.env.AION_MCP_PROXY_NODE_MODULES ||
  process.env.AION_NODE_MODULES ||
  "/Users/richard/coding-projects/github-repos/alfred-aion/node_modules";

const sdkBase = `${nodeModules}/@modelcontextprotocol/sdk/dist/esm`;

const [
  { Client },
  { StdioClientTransport },
  { Server },
  { StreamableHTTPServerTransport },
  {
    CallToolRequestSchema,
    GetPromptRequestSchema,
    ListPromptsRequestSchema,
    ListResourcesRequestSchema,
    ListResourceTemplatesRequestSchema,
    ListToolsRequestSchema,
    ReadResourceRequestSchema,
  },
] = await Promise.all([
  import(pathToFileURL(`${sdkBase}/client/index.js`)),
  import(pathToFileURL(`${sdkBase}/client/stdio.js`)),
  import(pathToFileURL(`${sdkBase}/server/index.js`)),
  import(pathToFileURL(`${sdkBase}/server/streamableHttp.js`)),
  import(pathToFileURL(`${sdkBase}/types.js`)),
]);

function parseJsonEnv(name, fallback) {
  const raw = process.env[name];
  if (!raw) return fallback;
  try {
    return JSON.parse(raw);
  } catch (error) {
    throw new Error(`${name} is not valid JSON: ${error.message}`);
  }
}

const name = process.env.AION_MCP_PROXY_NAME || "stdio-mcp";
const command = process.env.AION_MCP_PROXY_COMMAND;
const args = parseJsonEnv("AION_MCP_PROXY_ARGS_JSON", []);
const env = parseJsonEnv("AION_MCP_PROXY_ENV_JSON", {});
const port = Number(process.env.AION_MCP_PROXY_PORT || "0");
const host = process.env.AION_MCP_PROXY_HOST || "127.0.0.1";

if (!command) {
  throw new Error("AION_MCP_PROXY_COMMAND is required");
}

const upstreamTransport = new StdioClientTransport({
  command,
  args,
  env,
  stderr: "pipe",
});

upstreamTransport.stderr?.on("data", (chunk) => {
  process.stderr.write(`[${name}:stdio] ${chunk}`);
});

const client = new Client(
  {
    name: `aion-stdio-http-proxy:${name}`,
    version: "0.1.0",
  },
  { capabilities: {} },
);

await client.connect(upstreamTransport);

const server = new Server(
  {
    name: `aion-projected:${name}`,
    version: "0.1.0",
  },
  {
    capabilities: {
      tools: {},
      resources: {},
      prompts: {},
    },
  },
);

server.setRequestHandler(ListToolsRequestSchema, async (request) => client.listTools(request.params || {}));
server.setRequestHandler(CallToolRequestSchema, async (request) => client.callTool(request.params || {}));
server.setRequestHandler(ListResourcesRequestSchema, async (request) => client.listResources(request.params || {}));
server.setRequestHandler(ReadResourceRequestSchema, async (request) => client.readResource(request.params || {}));
server.setRequestHandler(ListResourceTemplatesRequestSchema, async (request) =>
  client.listResourceTemplates(request.params || {}),
);
server.setRequestHandler(ListPromptsRequestSchema, async (request) => client.listPrompts(request.params || {}));
server.setRequestHandler(GetPromptRequestSchema, async (request) => client.getPrompt(request.params || {}));

const transport = new StreamableHTTPServerTransport({
  sessionIdGenerator: randomUUID,
});
transport.onerror = (error) => {
  process.stderr.write(`[${name}:http] ${error?.stack || error?.message || String(error)}\n`);
};
await server.connect(transport);

const httpServer = http.createServer(async (req, res) => {
  if (req.url === "/health") {
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ ok: true, name }));
    return;
  }
  if (req.url !== "/mcp") {
    res.writeHead(404, { "content-type": "application/json" });
    res.end(JSON.stringify({ error: "not_found" }));
    return;
  }
  try {
    await transport.handleRequest(req, res);
  } catch (error) {
    if (!res.headersSent) {
      res.writeHead(500, { "content-type": "application/json" });
    }
    res.end(JSON.stringify({ error: error.message }));
  }
});

httpServer.listen(port, host, () => {
  const address = httpServer.address();
  const actualPort = typeof address === "object" && address ? address.port : port;
  process.stderr.write(`AION_MCP_PROXY_READY name=${name} url=http://${host}:${actualPort}/mcp\n`);
});

async function shutdown() {
  try {
    await transport.close();
  } catch {}
  try {
    await client.close();
  } catch {}
  httpServer.close(() => process.exit(0));
  setTimeout(() => process.exit(0), 1000).unref();
}

process.on("SIGTERM", shutdown);
process.on("SIGINT", shutdown);
