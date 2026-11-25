// Pre-publication scan for tokens, credentials, tailnet names, and real
// absolute paths. Tree mode scans every tracked file. --history scans every
// reachable blob, because a deleted file stays in the object graph. The
// patterns are written so this file does not match itself; the allowlist
// holds the documented example values and the fake test credentials.
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(fileURLToPath(new URL("../", import.meta.url)));
const allowFile = join(root, "tools", "secret-scan.allow");
const history = process.argv.includes("--history");
const maxPerRule = 20;
const tokenPrefix = "tk" + "_";
const devices = ["asus" + "vivos14", "galaxy" + "-s25", "lap1" + "user"].join("|");
const genericUsers = new Set(["example", "me", "you", "user", "username", "name", "real", "x", "test"]);
const fakePassword = /^(?:<|\$|fixture-|changeme|redacted|secret)/i;

const rules = [
  ["ntfy-token", new RegExp("\\b" + tokenPrefix + "[A-Za-z0-9]{8,}\\b", "g"), true],
  ["bearer", /\bBearer\s+[A-Za-z0-9._~+/=-]{16,}/g, true],
  ["basic", /\bBasic\s+[A-Za-z0-9+/=]{16,}/g, true],
  ["private-key", /-----BEGIN [A-Z ]*PRIVATE KEY-----/g, true],
  ["tailnet-dns", /\b(?:[a-z0-9][a-z0-9-]*\.)+ts\.net\b/g, false],
  ["tailnet-ip", /\b100\.(?:6[4-9]|[7-9][0-9]|1[01][0-9]|12[0-7])\.\d{1,3}\.\d{1,3}\b/g, false],
  ["tailnet-ipv6", /\bfd7a:115c:a1e0:[0-9a-f:]{2,}\b/g, false],
  ["windows-home", /[A-Za-z]:\\Users\\[^\\\s"'`]+/g, false],
  ["posix-home", /\/(?:home|Users)\/[a-z0-9][a-z0-9._-]*/g, false],
  ["wsl-mount", /\/mnt\/[a-z]\/Users\/[^\/\s"']+/g, false],
  ["device-name", new RegExp("\\b(?:" + devices + ")\\b", "g"), false],
  ["password", /"password"\s*:\s*"([^"]+)"/g, true],
];
const ruleIds = new Set(rules.map(([id]) => id));

function fail(message) {
  console.error("FAIL " + message);
  process.exit(2);
}

const allow = new Map();
for (const raw of readFileSync(allowFile, "utf8").split("\n")) {
  const line = raw.replace(/\r$/, "");
  if (line === "" || line.startsWith("#")) continue;
  const tab = line.indexOf("\t");
  if (tab < 1) fail("allowlist line without a tab: " + line);
  const id = line.slice(0, tab);
  if (!ruleIds.has(id)) fail("allowlist names an unknown rule: " + id);
  if (!allow.has(id)) allow.set(id, new Set());
  allow.get(id).add(line.slice(tab + 1));
}

const genericHome = (id, text) => {
  const parts = text.split(id === "windows-home" ? "\\" : "/");
  const user = id === "windows-home" ? parts[2] : id === "posix-home" ? parts[2] : parts[4];
  return genericUsers.has(user);
};

const skipMatch = (id, match) => {
  const allowed = allow.get(id) ?? new Set();
  if (id === "password") {
    const value = match[1];
    return value.length < 4 || (value.startsWith("{") && value.endsWith("}")) || fakePassword.test(value) || allowed.has(value);
  }
  if (id === "bearer" || id === "basic") {
    return allowed.has(match[0].split(/\s+/)[1]);
  }
  if (genericHome(id, match[0])) return true;
  return allowed.has(match[0]);
};

const hits = [];
let files = 0;
let blobs = 0;
let skipped = 0;

function scan(label, text) {
  const lines = text.split("\n");
  for (const [id, re, secret] of rules) {
    for (let i = 0; i < lines.length; i += 1) {
      const line = lines[i].replace(/\r$/, "");
      re.lastIndex = 0;
      let match;
      while ((match = re.exec(line)) !== null) {
        if (skipMatch(id, match)) continue;
        const value = secret ? match[0].slice(0, 8) + "..." : match[0].slice(0, 80);
        hits.push({ id, where: label + ":" + (i + 1), value });
      }
    }
  }
}

function treeScan() {
  const out = execFileSync("git", ["ls-files", "-z"], { cwd: root, encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] });
  for (const path of out.split("\0").filter(Boolean)) {
    const bytes = readFileSync(join(root, path));
    if (bytes.includes(0)) {
      skipped += 1;
      continue;
    }
    files += 1;
    scan(path, bytes.toString("utf8"));
  }
}

function historyScan() {
  const list = execFileSync("git", ["rev-list", "--objects", "--all"], { cwd: root, encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] });
  const paths = new Map();
  const oids = [];
  for (const line of list.split("\n")) {
    if (line === "") continue;
    const space = line.indexOf(" ");
    const oid = space === -1 ? line : line.slice(0, space);
    oids.push(oid);
    if (space !== -1 && !paths.has(oid)) paths.set(oid, line.slice(space + 1));
  }
  const batch = execFileSync("git", ["cat-file", "--batch"], {
    cwd: root,
    input: oids.join("\n") + "\n",
    maxBuffer: 512 * 1024 * 1024,
    stdio: ["pipe", "pipe", "ignore"],
  });
  let at = 0;
  while (at < batch.length) {
    const end = batch.indexOf(10, at);
    if (end === -1) break;
    const header = batch.subarray(at, end).toString("utf8").split(" ");
    at = end + 1;
    if (header.length < 3) continue;
    const size = Number(header[2]);
    const content = batch.subarray(at, at + size);
    at += size + 1;
    if (header[1] !== "blob" || content.includes(0)) {
      if (header[1] === "blob") skipped += 1;
      continue;
    }
    blobs += 1;
    scan("blob " + header[0].slice(0, 10) + " " + (paths.get(header[0]) ?? "(unnamed)"), content.toString("utf8"));
  }
}

treeScan();
if (history) historyScan();

const scope = (history ? files + " files, " + blobs + " history blobs" : files + " files") + (skipped > 0 ? ", " + skipped + " binary skipped" : "");
if (hits.length === 0) {
  console.log("PASS secrets: " + scope + ", 0 hits");
  process.exit(0);
}

console.error("FAIL secrets: " + scope + ", " + hits.length + " hits");
for (const [id] of rules) {
  const group = hits.filter((hit) => hit.id === id);
  if (group.length === 0) continue;
  console.error("  " + id + " (" + group.length + ")");
  for (const hit of group.slice(0, maxPerRule)) console.error("    " + hit.where + "  " + hit.value);
  if (group.length > maxPerRule) console.error("    ... " + (group.length - maxPerRule) + " more");
}
process.exit(1);
