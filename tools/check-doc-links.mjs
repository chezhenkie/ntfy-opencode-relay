// Checks every relative markdown link in the tracked docs and the printable
// ASCII rule from the repository conventions. Exit 0 only when both hold.
// The file list comes from git, so an untracked note in the tree neither
// fails the gate nor hides a doc that is committed.
import { execFileSync } from "node:child_process";
import { existsSync, readFileSync, readdirSync, statSync } from "node:fs";
import { dirname, extname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(fileURLToPath(new URL("../", import.meta.url)));
const skip = new Set([".git", ".scratch", ".ntfy-inbox", "node_modules", "resources", "logs", "temp", "dist"]);

const walk = (dir) => {
  const out = [];
  for (const entry of readdirSync(dir)) {
    if (skip.has(entry)) continue;
    const full = join(dir, entry);
    if (statSync(full).isDirectory()) out.push(...walk(full));
    else if (extname(full) === ".md") out.push(full);
  }
  return out;
};

const tracked = () => {
  try {
    const out = execFileSync("git", ["ls-files", "-z", "--", "*.md"], { cwd: root, encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] });
    const list = out.split("\0").filter(Boolean).map((p) => join(root, p));
    return list.length > 0 ? list : null;
  } catch {
    return null;
  }
};

const slug = (heading) => {
  const seen = new Map();
  const base = heading
    .toLowerCase()
    .replace(/`/g, "")
    .replace(/[^\p{L}\p{N}\s-]/gu, "")
    .trim()
    .replace(/\s+/g, "-");
  return base;
};

const anchorsOf = (file) => {
  const text = readFileSync(file, "utf8");
  const counts = new Map();
  const out = new Set();
  let fence = false;
  for (const line of text.split(/\r?\n/)) {
    if (/^\s*(```|~~~)/.test(line)) fence = !fence;
    if (fence) continue;
    const m = /^#{1,6}\s+(.+?)\s*$/.exec(line);
    if (!m) continue;
    const base = slug(m[1]);
    const n = counts.get(base) ?? 0;
    counts.set(base, n + 1);
    out.add(n === 0 ? base : `${base}-${n}`);
  }
  return out;
};

const failures = [];
const files = tracked() ?? walk(root);
for (const file of files) {
  const text = readFileSync(file, "utf8");
  const rel = relative(root, file).replace(/\\/g, "/");

  const bad = [...text].filter((ch) => ch !== "\n" && ch !== "\r" && ch !== "\t" && (ch.codePointAt(0) < 0x20 || ch.codePointAt(0) > 0x7e));
  if (bad.length > 0) failures.push(`${rel}: ${bad.length} non-ASCII characters, first ${JSON.stringify(bad[0])}`);

  const lines = text.split(/\r?\n/);
  lines.forEach((line, index) => {
    for (const match of line.matchAll(/\]\(([^)\s]+)\)/g)) {
      const target = match[1];
      if (/^[a-z]+:/i.test(target) || target.startsWith("//")) continue;
      const hash = target.indexOf("#");
      const pathPart = hash === -1 ? target : target.slice(0, hash);
      const anchor = hash === -1 ? null : decodeURIComponent(target.slice(hash + 1)).toLowerCase();
      const targetPath = pathPart === "" ? file : resolve(dirname(file), decodeURIComponent(pathPart));
      if (!existsSync(targetPath)) {
        failures.push(`${rel}:${index + 1}: missing ${target}`);
        continue;
      }
      if (anchor === null) continue;
      const anchors = anchorsOf(targetPath);
      if (!anchors.has(anchor)) {
        failures.push(`${rel}:${index + 1}: missing anchor #${anchor} in ${relative(root, targetPath).replace(/\\/g, "/")}`);
      }
    }
  });
}

if (failures.length > 0) {
  for (const failure of failures) console.log(`FAIL ${failure}`);
  console.log(`FAIL ${failures.length} problem(s) in ${files.length} markdown files`);
  process.exit(1);
}
console.log(`PASS ${files.length} markdown files, every relative link resolves, all ASCII`);
