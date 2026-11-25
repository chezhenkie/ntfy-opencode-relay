// Rebuild the published pages under docs\github-page-html-files.
//
// Steps: convert every chapter and specsheet page into the docs folder, move
// the outputs into the published folder, point their .md links at the .html
// siblings, and check that every local link resolves. The hand-built site
// index at docs\index.html is preserved: the converter would overwrite it from
// docs\index.md.
//
// The converter is a local tool. Set FOTON_MD_HTML to its path; the default is
// the opencode scripts copy.
import { spawnSync } from "node:child_process";
import { copyFileSync, existsSync, mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, "..");
const docs = join(repo, "docs");
const published = join(docs, "github-page-html-files");
const converter =
  process.env.FOTON_MD_HTML ||
  join(
    process.env.USERPROFILE || "",
    ".config",
    "opencode",
    "scripts",
    "foton-md-html-converter-x64.exe",
  );

if (!existsSync(converter)) {
  console.error(`FAIL converter not found: ${converter}`);
  process.exit(1);
}

const index = join(docs, "index.html");
const indexBackup = join(docs, "index.handbuilt.tmp.html");
const hadIndex = existsSync(index);
if (hadIndex) copyFileSync(index, indexBackup);

function convert(target) {
  const run = spawnSync(converter, ["--target", target], { encoding: "utf8" });
  if (run.status !== 0) {
    console.error(`FAIL convert ${target}\n${run.stdout ?? ""}${run.stderr ?? ""}`);
    process.exit(1);
  }
  return (run.stdout || "").trim().split("\n").pop();
}

function movePages(from, to) {
  mkdirSync(to, { recursive: true });
  let moved = 0;
  for (const entry of readdirSync(from, { withFileTypes: true })) {
    if (!entry.isFile() || !entry.name.endsWith(".html")) continue;
    if (from === docs && entry.name === "index.html") continue;
    copyFileSync(join(from, entry.name), join(to, entry.name));
    rmSync(join(from, entry.name));
    moved += 1;
  }
  return moved;
}

const chapterNote = convert(docs);
const specsheetNote = convert(join(docs, "specsheet"));

if (hadIndex) {
  copyFileSync(indexBackup, index);
  rmSync(indexBackup);
}

const chapters = movePages(docs, published);
const specsheets = movePages(join(docs, "specsheet"), join(published, "specsheet"));

let rewritten = 0;
function relink(dir) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) {
      relink(full);
      continue;
    }
    if (!entry.name.endsWith(".html")) continue;
    const before = readFileSync(full, "utf8");
    const after = before.replace(/href="([^"]+?)\.md(#[^"]*)?"/g, (match, base, anchor) => {
      if (/^(https?:|\/|\.\.\/)/.test(base)) return match;
      rewritten += 1;
      return `href="${base}.html${anchor || ""}"`;
    });
    if (after !== before) writeFileSync(full, after);
  }
}

relink(published);

let links = 0;
const missing = [];
function check(dir) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) {
      check(full);
      continue;
    }
    if (!entry.name.endsWith(".html")) continue;
    const text = readFileSync(full, "utf8");
    for (const match of text.matchAll(/href="([^"]+)"/g)) {
      const target = match[1];
      if (/^(https?:|mailto:|#|\/)/.test(target)) continue;
      const clean = target.split("#")[0].split("?")[0];
      if (!clean) continue;
      links += 1;
      if (!existsSync(resolve(dirname(full), clean))) {
        missing.push(`${relative(published, full)} -> ${target}`);
      }
    }
  }
}

check(published);

console.log(`chapters=${chapters} specsheet=${specsheets} relinked=${rewritten}`);
if (chapterNote) console.log(`  ${chapterNote}`);
if (specsheetNote) console.log(`  ${specsheetNote}`);
console.log(`links=${links} missing=${missing.length}`);
for (const line of missing) console.log(`  MISSING ${line}`);
process.exit(missing.length === 0 ? 0 : 1);
