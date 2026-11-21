// Generate packaging/THIRD_PARTY_LICENSES.txt: the license notices of every
// Rust crate linked into the shipped binaries (audivane-dsp, avdsp,
// audivane-dsp-ui) and every production npm package bundled into the app UI.
//
//   node tools/third-party-notices.mjs           # write the file
//   node tools/third-party-notices.mjs --check   # fail if it is out of date
//
// Rust: `cargo metadata --locked` with the release features, following normal
// (non-dev, non-build) dependency edges from the three shipped crates; build
// scripts and proc-macros, which do not end up in the binaries, are skipped.
// `cargo metadata` also lists optional dependencies whose feature is off
// (e.g. tauri's tray-icon/libappindicator), so only crates that `cargo tree`
// reports for the same packages, features and target are kept.
// npm: `npm ls --omit=dev` in app/. For each package the license expression
// and every top-level LICENSE / COPYING / NOTICE / COPYRIGHT file are
// included; identical texts are printed once and list every package using
// them. Needs the cargo registry sources and app/node_modules (run after a
// build). Bundled fonts and target data have their own records:
// docs/font-licensing.md and app/src/assets/*/SOURCES.md.
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, readdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const OUT = path.join(ROOT, "packaging/THIRD_PARTY_LICENSES.txt");
const SHIPPED = ["audivane-dsp-daemon", "audivane-dsp-cli", "audivane-dsp-ui"];
const LICENSE_FILE = /^(licen[cs]e|copying|notice|copyright|unlicense)([-._].*)?$/i;

function licenseFiles(dir) {
  return readdirSync(dir)
    .filter((name) => LICENSE_FILE.test(name) && statSync(path.join(dir, name)).isFile())
    .sort()
    .map((name) => ({ name, text: readFileSync(path.join(dir, name), "utf8").replace(/\r\n/g, "\n").trim() }));
}

// A crate that ships no license file gets the standard text of its declared
// MIT or BSD-3-Clause license with its Cargo authors as the copyright holders.
// Apache-2.0 and MPL-2.0 carry no per-project copyright line; their full texts
// are already among the collected texts.
const MIT = (holders) => `Copyright (c) ${holders}

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.`;
const BSD3 = (holders) => `Copyright (c) ${holders}

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice, this
   list of conditions and the following disclaimer.

2. Redistributions in binary form must reproduce the above copyright notice,
   this list of conditions and the following disclaimer in the documentation
   and/or other materials provided with the distribution.

3. Neither the name of the copyright holder nor the names of its contributors
   may be used to endorse or promote products derived from this software
   without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND
ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.`;
function standardText(p) {
  const holders = (p.authors ?? []).map((a) => a.replace(/\s*<[^>]*>/, "")).join(", ") || `the ${p.name} authors`;
  const note = "standard text; the crate package ships no license file";
  if (p.license === "MIT") return [{ name: `MIT (${note})`, text: MIT(holders) }];
  if (p.license === "BSD-3-Clause") return [{ name: `BSD-3-Clause (${note})`, text: BSD3(holders) }];
  return [];
}

/** "name version" of every crate `cargo tree` says the shipped binaries
 *  build with (normal edges, the release features, the Linux target). */
function builtCrates() {
  const out = execFileSync("cargo", [
    "tree", "--locked", "-e", "normal", "--target", "x86_64-unknown-linux-gnu",
    ...SHIPPED.flatMap((p) => ["-p", p]),
    "--features", "audivane-dsp-ui/custom-protocol",
    "--prefix", "none", "-f", "{p}",
  ], { cwd: ROOT, maxBuffer: 64 * 1024 * 1024, encoding: "utf8" });
  const set = new Set();
  for (const line of out.split("\n")) {
    const m = line.trim().match(/^(\S+) v(\S+)/);
    if (m) set.add(`${m[1]} ${m[2]}`);
  }
  if (set.size === 0) throw new Error("cargo tree listed no crates");
  return set;
}

function rustPackages() {
  const built = builtCrates();
  const meta = JSON.parse(execFileSync("cargo", [
    "metadata", "--format-version", "1", "--locked",
    "--filter-platform", "x86_64-unknown-linux-gnu",
    "--features", "audivane-dsp-ui/custom-protocol",
  ], { cwd: ROOT, maxBuffer: 256 * 1024 * 1024, encoding: "utf8" }));
  const byId = new Map(meta.packages.map((p) => [p.id, p]));
  const nodes = new Map(meta.resolve.nodes.map((n) => [n.id, n]));
  const workspace = new Set(meta.workspace_members);
  const start = meta.packages.filter((p) => SHIPPED.includes(p.name) && workspace.has(p.id)).map((p) => p.id);
  if (start.length !== SHIPPED.length) throw new Error(`shipped crates not found: ${SHIPPED.join(", ")}`);
  const seen = new Set(start);
  const queue = [...start];
  while (queue.length > 0) {
    const node = nodes.get(queue.shift());
    for (const dep of node?.deps ?? []) {
      if (!dep.dep_kinds.some((k) => k.kind === null)) continue;
      const pkg = byId.get(dep.pkg);
      if (pkg.targets.some((t) => t.kind.includes("proc-macro"))) continue;
      if (!built.has(`${pkg.name} ${pkg.version}`)) continue;
      if (!seen.has(dep.pkg)) { seen.add(dep.pkg); queue.push(dep.pkg); }
    }
  }
  return [...seen].filter((id) => !workspace.has(id)).map((id) => {
    const p = byId.get(id);
    const dir = path.dirname(p.manifest_path);
    const files = licenseFiles(dir);
    if (p.license_file && !files.some((f) => path.join(dir, f.name) === path.join(dir, p.license_file))) {
      const file = path.join(dir, p.license_file);
      if (existsSync(file)) files.push({ name: p.license_file, text: readFileSync(file, "utf8").replace(/\r\n/g, "\n").trim() });
    }
    if (files.length === 0) files.push(...standardText(p));
    return { eco: "crate", name: p.name, version: p.version, license: p.license ?? "(see license file)", url: p.repository ?? "", files };
  });
}

function npmPackages() {
  const app = path.join(ROOT, "app");
  const tree = JSON.parse(execFileSync("npm", ["ls", "--omit=dev", "--all", "--json", "--long"], { cwd: app, maxBuffer: 64 * 1024 * 1024, encoding: "utf8" }));
  const out = new Map();
  const walk = (deps) => {
    for (const [name, d] of Object.entries(deps ?? {})) {
      const key = `${name}@${d.version}`;
      if (!out.has(key) && d.path) {
        const pkg = JSON.parse(readFileSync(path.join(d.path, "package.json"), "utf8"));
        const repo = typeof pkg.repository === "string" ? pkg.repository : pkg.repository?.url ?? "";
        out.set(key, { eco: "npm", name, version: d.version, license: pkg.license ?? "(see license file)", url: repo.replace(/^git\+/, ""), files: licenseFiles(d.path) });
      }
      walk(d.dependencies);
    }
  };
  walk(tree.dependencies);
  return [...out.values()];
}

function vendoredPackages() {
  return [
    {
      eco: "c++",
      name: "signalsmith-stretch",
      version: "1.3.1",
      license: "MIT",
      url: "https://github.com/Signalsmith-Audio/signalsmith-stretch",
      files: licenseFiles(path.join(ROOT, "crates/dsp-core/third_party/signalsmith-stretch")),
    },
    {
      eco: "c++",
      name: "signalsmith-linear",
      version: "vendored",
      license: "MIT",
      url: "https://github.com/Signalsmith-Audio/linear",
      files: licenseFiles(path.join(ROOT, "crates/dsp-core/third_party/signalsmith-linear")),
    },
  ];
}

const packages = [...rustPackages(), ...npmPackages(), ...vendoredPackages()]
  .sort((a, b) => a.eco.localeCompare(b.eco) || a.name.localeCompare(b.name) || a.version.localeCompare(b.version, undefined, { numeric: true }));

const missing = packages.filter((p) => p.files.length === 0);
const texts = new Map(); // hash -> { text, users: [] }
for (const p of packages) {
  for (const f of p.files) {
    const h = createHash("sha256").update(f.text).digest("hex");
    if (!texts.has(h)) texts.set(h, { text: f.text, users: [] });
    texts.get(h).users.push(`${p.name} ${p.version} (${f.name})`);
  }
}

const rule = "=".repeat(78);
let doc = `Audivane DSP: third-party software notices

Audivane DSP is licensed under the GNU Affero General Public License v3.0 only.
Its binaries (audivane-dsp, avdsp, audivane-dsp-ui) include the third-party
software listed below, each under its own license. The fonts and target-curve
data bundled in the app have their own notices: OFL-IBM-Plex.txt,
font-licensing.md and the targets SOURCES.md files installed beside this file.

The source code of every package is available from crates.io or npmjs.com at
the version listed, and from the repository shown; this includes the Source
Code Form of the MPL-2.0 packages.

Generated by tools/third-party-notices.mjs from Cargo.lock and
app/package-lock.json. ${packages.length} packages, ${texts.size} distinct license texts.

${rule}
PACKAGES
${rule}

`;
for (const p of packages) {
  doc += `${p.eco.padEnd(5)}  ${p.name} ${p.version}  —  ${p.license}${p.url ? `\n       ${p.url}` : ""}\n`;
}
if (missing.length > 0) {
  doc += `\nThese packages ship no license file of their own. They are distributed
under the license named above (for a choice of licenses, under Apache-2.0),
whose full standard text appears among the texts below:
${missing.map((p) => `  ${p.name} ${p.version} (${p.license})`).join("\n")}\n`;
}
doc += `\n${rule}
ALGORITHMS
${rule}

The headphone crossfeed's bs2b modes implement the filter design published
by the bs2b project (Bauer stereophonic-to-binaural DSP,
https://bs2b.sourceforge.net/). No bs2b code is included; the design is
credited here. libbs2b is distributed under the MIT License:

  Copyright (c) 2005 Boris Mikhaylov

  Permission is hereby granted, free of charge, to any person obtaining a
  copy of this software and associated documentation files (the
  "Software"), to deal in the Software without restriction, including
  without limitation the rights to use, copy, modify, merge, publish,
  distribute, sublicense, and/or sell copies of the Software, and to permit
  persons to whom the Software is furnished to do so, subject to the
  following conditions:

  The above copyright notice and this permission notice shall be included
  in all copies or substantial portions of the Software.

  THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
  OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
  MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
  NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM,
  DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR
  OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE
  USE OR OTHER DEALINGS IN THE SOFTWARE.
`;
doc += `\n${rule}\nLICENSE TEXTS\n${rule}\n`;
const ordered = [...texts.values()].sort((a, b) => a.users[0].localeCompare(b.users[0]));
for (const t of ordered) {
  doc += `\n${"-".repeat(78)}\nUsed by: ${t.users.join(", ")}\n${"-".repeat(78)}\n\n${t.text}\n`;
}

if (process.argv.includes("--check")) {
  const current = existsSync(OUT) ? readFileSync(OUT, "utf8") : "";
  if (current !== doc) {
    console.error(`[notices] ${path.relative(ROOT, OUT)} is out of date; run node tools/third-party-notices.mjs`);
    process.exit(1);
  }
  console.log(`[notices] ${path.relative(ROOT, OUT)} is current (${packages.length} packages)`);
} else {
  writeFileSync(OUT, doc);
  console.log(`[notices] wrote ${path.relative(ROOT, OUT)}: ${packages.length} packages, ${texts.size} texts, ${missing.length} without a license file, ${(doc.length / 1024).toFixed(0)} KiB`);
  if (missing.length > 0) console.log(`  without file: ${missing.map((p) => `${p.name} (${p.license})`).join(", ")}`);
}
