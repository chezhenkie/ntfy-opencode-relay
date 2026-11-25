#!/usr/bin/env node
// End-to-end smoke against the live ntfy server: health, the auth gate,
// header and JSON publish, poll read-back, SSE delivery, and an attachment
// round trip. The suite speaks only to ntfy over its HTTP API.
//
// The container, host route, and relay process are covered by
// tools\startup-check.ps1 and ntfy-deploy status; this suite proves the wire
// the relay and the phone actually use.

import fs from 'node:fs';
import path from 'node:path';
import { randomBytes } from 'node:crypto';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(here, '..');
const DEFAULT_TOPIC = 'e2e-smoke';
const DEFAULT_TIMEOUT_MS = 10000;
const OUT_TAG = 'oc-out';

function usage() {
  console.log(`usage: node smoke.mjs [options]

  --base-url <url>     ntfy base URL (env NTFY_BASE_URL, default http://127.0.0.1:8810)
  --token-file <path>  bearer token file (env NTFY_TOKEN_FILE, else the relay root)
  --topic <name>       test topic (default ${DEFAULT_TOPIC})
  --timeout <ms>       per-request timeout (default ${DEFAULT_TIMEOUT_MS})
  --help               print this text`);
}

function parseArgs(argv) {
  const options = {
    baseUrl: process.env.NTFY_BASE_URL || 'http://127.0.0.1:8810',
    tokenFile: process.env.NTFY_TOKEN_FILE || '',
    topic: DEFAULT_TOPIC,
    timeoutMs: DEFAULT_TIMEOUT_MS,
  };
  const value = (index, flag) => {
    const raw = argv[index];
    if (raw === undefined || raw.startsWith('--')) {
      throw new Error(`${flag} needs a value`);
    }
    return raw;
  };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === '--help' || arg === '-h') {
      usage();
      process.exit(0);
    } else if (arg === '--base-url') {
      options.baseUrl = value((index += 1), arg);
    } else if (arg === '--token-file') {
      options.tokenFile = value((index += 1), arg);
    } else if (arg === '--topic') {
      options.topic = value((index += 1), arg);
    } else if (arg === '--timeout') {
      const raw = value((index += 1), arg);
      const parsed = Number.parseInt(raw, 10);
      if (!Number.isFinite(parsed) || parsed <= 0) {
        throw new Error(`--timeout must be a positive number, got ${raw}`);
      }
      options.timeoutMs = parsed;
    } else {
      throw new Error(`unknown argument ${arg}`);
    }
  }
  const base = new URL(options.baseUrl);
  if (base.protocol !== 'http:' && base.protocol !== 'https:') {
    throw new Error(`unsupported base URL scheme ${base.protocol}`);
  }
  if (base.search || base.hash) {
    throw new Error('base URL must not carry a query or fragment');
  }
  options.baseUrl = base.toString().replace(/\/$/, '');
  if (!/^[A-Za-z0-9_-]{1,64}$/.test(options.topic)) {
    throw new Error(`invalid topic ${options.topic}`);
  }
  return options;
}

function tokenFileDefault() {
  if (process.env.NTFY_TOKEN_FILE) return path.resolve(process.env.NTFY_TOKEN_FILE);
  if (process.env.NTFY_RELAY_HOME) return path.join(process.env.NTFY_RELAY_HOME, 'ntfy-token.txt');
  if (process.env.LOCALAPPDATA) return path.join(process.env.LOCALAPPDATA, 'ntfy-relay', 'ntfy-token.txt');
  return path.join(repoRoot, 'bin', 'ntfy-relay', 'ntfy-token.txt');
}

function readToken(file) {
  const resolved = file || tokenFileDefault();
  let raw;
  try {
    raw = fs.readFileSync(resolved, 'utf8');
  } catch (error) {
    throw new Error(`cannot read token file ${resolved}: ${error.code || error.message}`);
  }
  const token = raw.trim();
  if (!token || /\s/.test(token)) {
    throw new Error(`token file ${resolved} is empty or holds whitespace`);
  }
  return token;
}

function short(value) {
  return String(value && value.message ? value.message : value).replace(/\s+/g, ' ').slice(0, 300);
}

function assert(condition, message) {
  if (!condition) throw new Error(message);
}

function marker(label) {
  return `e2e-${label}-${Date.now().toString(36)}-${randomBytes(3).toString('hex')}`;
}

function encodeTopic(topic) {
  return encodeURIComponent(topic);
}

async function request(method, url, options = {}) {
  const headers = { ...(options.headers || {}) };
  if (options.token) headers.authorization = `Bearer ${options.token}`;
  let response;
  try {
    response = await fetch(url, {
      method,
      headers,
      body: options.body,
      redirect: 'manual',
      signal: AbortSignal.timeout(options.timeoutMs || DEFAULT_TIMEOUT_MS),
    });
  } catch (error) {
    throw new Error(`${method} ${url} failed: ${short(error)}`);
  }
  const bytes = new Uint8Array(await response.arrayBuffer());
  return {
    status: response.status,
    headers: response.headers,
    bytes,
    body: new TextDecoder().decode(bytes),
  };
}

function parseJson(response, label) {
  try {
    return JSON.parse(response.body);
  } catch {
    throw new Error(`${label} returned a non-JSON body`);
  }
}

async function poll(ctx, since) {
  const url = `${ctx.baseUrl}/${encodeTopic(ctx.topic)}/json?poll=1&since=${since}`;
  const response = await request('GET', url, { token: ctx.token, timeoutMs: ctx.timeoutMs });
  assert(response.status === 200, `poll returned ${response.status}`);
  const rows = [];
  for (const line of response.body.split('\n')) {
    if (!line.trim()) continue;
    try {
      rows.push(JSON.parse(line));
    } catch {
      throw new Error('poll returned a non-JSON row');
    }
  }
  return rows;
}

async function waitForSse(ctx, message) {
  const controller = new AbortController();
  const timeout = Math.max(ctx.timeoutMs, 10000);
  const timer = setTimeout(() => controller.abort(), timeout);
  const since = Math.floor(Date.now() / 1000) - 2;
  const url = `${ctx.baseUrl}/${encodeTopic(ctx.topic)}/sse?since=${since}`;
  let published = false;
  const publishNow = async () => {
    if (published) return;
    published = true;
    const response = await request('POST', `${ctx.baseUrl}/${encodeTopic(ctx.topic)}`, {
      token: ctx.token,
      headers: { Title: 'e2e smoke sse', Tags: OUT_TAG, 'content-type': 'text/plain;charset=UTF-8' },
      body: message,
      timeoutMs: ctx.timeoutMs,
    });
    assert(response.status === 200, `publish before sse returned ${response.status}`);
  };
  const fallback = setTimeout(() => {
    publishNow().catch(() => {});
  }, 1000);
  try {
    const response = await fetch(url, {
      headers: { authorization: `Bearer ${ctx.token}`, accept: 'text/event-stream' },
      signal: controller.signal,
    });
    assert(response.status === 200, `sse returned ${response.status}`);
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let buffer = '';
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true }).replace(/\r/g, '');
      for (;;) {
        const split = buffer.indexOf('\n\n');
        if (split < 0) break;
        const frame = buffer.slice(0, split);
        buffer = buffer.slice(split + 2);
        const data = frame
          .split('\n')
          .filter((line) => line.startsWith('data:'))
          .map((line) => line.slice(5).trimStart())
          .join('\n');
        if (!data) continue;
        let event;
        try {
          event = JSON.parse(data);
        } catch {
          continue;
        }
        if (event.event === 'open') await publishNow();
        if (event.event === 'message' && event.topic === ctx.topic && event.message === message) {
          return event;
        }
      }
    }
    throw new Error('sse closed before the message arrived');
  } catch (error) {
    if (error && error.name === 'AbortError') {
      throw new Error(`no sse frame within ${timeout} ms`);
    }
    throw error;
  } finally {
    clearTimeout(timer);
    clearTimeout(fallback);
    controller.abort();
  }
}

const checks = [];
function check(name, run) {
  checks.push({ name, run });
}

check('health-anonymous', async (ctx) => {
  const response = await request('GET', `${ctx.baseUrl}/v1/health`, { timeoutMs: ctx.timeoutMs });
  assert(response.status === 200, `status ${response.status}`);
  const body = parseJson(response, 'health');
  assert(body.healthy === true, 'healthy is not true');
  return `healthy=true role=${body.role || '-'}`;
});

check('publish-denied-anonymous', async (ctx) => {
  const probe = marker('anon-publish');
  const response = await request('POST', `${ctx.baseUrl}/${encodeTopic(ctx.topic)}`, {
    headers: { Title: 'e2e smoke', 'content-type': 'text/plain;charset=UTF-8' },
    body: probe,
    timeoutMs: ctx.timeoutMs,
  });
  assert(response.status === 403, `expected 403, got ${response.status}`);
  const rows = await poll(ctx, Math.floor(Date.now() / 1000) - 60);
  assert(!rows.some((row) => row.message === probe), 'anonymous message was stored');
  return 'status=403';
});

check('read-denied-anonymous', async (ctx) => {
  const url = `${ctx.baseUrl}/${encodeTopic(ctx.topic)}/json?poll=1&since=0`;
  const response = await request('GET', url, { timeoutMs: ctx.timeoutMs });
  assert(response.status === 401 || response.status === 403, `expected 401 or 403, got ${response.status}`);
  return `status=${response.status}`;
});

check('publish-header', async (ctx) => {
  const body = marker('header');
  const response = await request('POST', `${ctx.baseUrl}/${encodeTopic(ctx.topic)}`, {
    token: ctx.token,
    headers: { Title: 'e2e smoke', Tags: OUT_TAG, 'content-type': 'text/plain;charset=UTF-8' },
    body,
    timeoutMs: ctx.timeoutMs,
  });
  assert(response.status === 200, `status ${response.status}`);
  const message = parseJson(response, 'publish');
  assert(typeof message.id === 'string' && message.id.length === 12, 'message id is not 12 characters');
  assert(message.topic === ctx.topic, `topic is ${message.topic}`);
  assert(message.message === body, 'message body differs');
  ctx.headerMessage = { ...message, expectedBody: body };
  return `id=${message.id}`;
});

check('read-back', async (ctx) => {
  const reference = ctx.headerMessage;
  assert(reference, 'publish-header did not produce a message');
  const since = Math.max(0, reference.time - 30);
  const rows = await poll(ctx, since);
  const hit = rows.find((row) => row.id === reference.id);
  assert(hit, `id ${reference.id} missing from ${rows.length} poll rows`);
  assert(hit.message === reference.expectedBody, 'message body differs');
  assert(hit.title === reference.title, 'title differs');
  assert(Array.isArray(hit.tags) && hit.tags.includes(OUT_TAG), `tag ${OUT_TAG} missing`);
  return `id=${hit.id} rows=${rows.length}`;
});

check('sse-live', async (ctx) => {
  const body = marker('sse');
  const started = Date.now();
  const event = await waitForSse(ctx, body);
  assert(event.event === 'message', `event is ${event.event}`);
  return `id=${event.id} frame in ${Date.now() - started} ms`;
});

check('publish-json', async (ctx) => {
  const body = marker('json');
  const title = 'e2e smoke json';
  const response = await request('POST', `${ctx.baseUrl}/`, {
    token: ctx.token,
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ topic: ctx.topic, title, message: body, priority: 3, tags: [OUT_TAG] }),
    timeoutMs: ctx.timeoutMs,
  });
  assert(response.status === 200, `status ${response.status}`);
  const published = parseJson(response, 'json publish');
  assert(published.topic === ctx.topic, `topic is ${published.topic}`);
  const rows = await poll(ctx, Math.max(0, published.time - 30));
  const hit = rows.find((row) => row.id === published.id);
  assert(hit, `id ${published.id} missing from ${rows.length} poll rows`);
  assert(hit.message === body, 'message body differs');
  assert(hit.title === title, 'title differs');
  return `id=${published.id} priority=${published.priority}`;
});

check('attachment-roundtrip', async (ctx) => {
  const name = `e2e-${randomBytes(4).toString('hex')}.txt`;
  const payload = Buffer.from(`e2e attachment ${marker('file')}\n`, 'ascii');
  const query = `filename=${encodeURIComponent(name)}&title=${encodeURIComponent('e2e smoke file')}`;
  const response = await request('PUT', `${ctx.baseUrl}/${encodeTopic(ctx.topic)}?${query}`, {
    token: ctx.token,
    headers: { 'content-type': 'text/plain;charset=utf-8' },
    body: payload,
    timeoutMs: ctx.timeoutMs,
  });
  assert(response.status === 200, `upload returned ${response.status}`);
  const message = parseJson(response, 'upload');
  const attachment = message.attachment;
  assert(attachment, 'upload response has no attachment');
  assert(attachment.name === name, `attachment name is ${attachment.name}`);
  assert(attachment.size === payload.length, `attachment size is ${attachment.size}`);
  const fileUrl = new URL(attachment.url);
  const fetchUrl = new URL(fileUrl.pathname + fileUrl.search, ctx.baseUrl).toString();
  const file = await request('GET', fetchUrl, { timeoutMs: ctx.timeoutMs });
  assert(file.status === 200, `download returned ${file.status}`);
  assert(Buffer.from(file.bytes).equals(payload), 'downloaded bytes differ');
  return `bytes=${payload.length} host=${fileUrl.host}`;
});

async function main() {
  const options = parseArgs(process.argv.slice(2));
  const token = readToken(options.tokenFile);
  const ctx = {
    baseUrl: options.baseUrl,
    token,
    topic: options.topic,
    timeoutMs: options.timeoutMs,
  };
  console.log(`ntfy e2e smoke base=${ctx.baseUrl} topic=${ctx.topic}`);
  let failures = 0;
  for (const { name, run } of checks) {
    const started = Date.now();
    try {
      const detail = await run(ctx);
      const elapsed = Date.now() - started;
      console.log(`PASS ${name}${detail ? ` ${detail}` : ''} (${elapsed} ms)`);
    } catch (error) {
      failures += 1;
      const elapsed = Date.now() - started;
      console.log(`FAIL ${name} ${short(error)} (${elapsed} ms)`);
    }
  }
  console.log(failures === 0 ? `RESULT ${checks.length} passed` : `RESULT ${failures} of ${checks.length} failed`);
  process.exitCode = failures === 0 ? 0 : 1;
}

main().catch((error) => {
  console.log(`FAIL setup ${short(error)}`);
  process.exit(2);
});
