// Verify the bundled font subsets against third_party/fonts.json.
//
// The app bundles a Latin subset of Geist (interface text) and Latin subsets
// of IBM Plex Mono (numbers and values). IBM Plex declares the Reserved Font
// Name "Plex", so its subsets ship renamed: their primary name records carry
// Audivane Mono Latin while copyright, license URL, glyphs and metrics are
// unchanged. Geist has no Reserved Font Name and keeps its own names; its
// subset is recorded with the upstream file, axis range, code points and
// command that made it (the `subset` record in the manifest).
//
// Decodes each WOFF2 with no third-party dependencies and confirms: the
// manifest inventory matches the bundled files exactly with its license,
// family policies and pinned fonttools version intact; the recorded file
// hashes; primary name records based on the recorded family and free of the
// reserved name, plus the preserved copyright, OFL license URL and PostScript
// name; each family's OFL notice beside the fonts; and a content fingerprint
// over every stored WOFF2 table except `name` and the head table's
// checkSumAdjustment that must equal the fingerprint recorded from the
// original upstream subset (originalContentSha256), so the glyph and metric
// data are proven identical to the originals. A `subset` family (Geist) has no
// upstream subset to compare with: its bytes and fingerprint must match the
// recorded sha256 and contentSha256 of the file made by the recorded command. Reproduce an original
// fingerprint with `node tools/check-subset-fonts.mjs --fingerprint <file>`.
//
// `--update` recomputes contentSha256 from disk; tools/rename-subset-fonts.py
// invokes it to finish a regeneration. Run directly as
// `node tools/check-subset-fonts.mjs`; the release pipeline runs it too.

import { createHash } from "node:crypto";
import { readFileSync, readdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { brotliDecompressSync, brotliCompressSync } from "node:zlib";

export const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
export const FONT_MANIFEST = "third_party/fonts.json";
// Read by tools/rename-subset-fonts.py together with RENAMED_NAME_IDS, so
// both tools share this single source of truth.
export const FONT_DIRECTORIES = [
  "app/src/assets/fonts",
];

// WOFF2 specification, Known Table Tags (W3C REC-WOFF2, subclause 4.1).
const KNOWN_TAGS = [
  "cmap", "head", "hhea", "hmtx", "maxp", "name", "OS/2", "post", "cvt ",
  "fpgm", "glyf", "loca", "prep", "CFF ", "VORG", "EBDT", "EBLC", "gasp",
  "hdmx", "kern", "LTSH", "PCLT", "VDMX", "vhea", "vmtx", "BASE", "GDEF",
  "GPOS", "GSUB", "EBSC", "JSTF", "MATH", "CBDT", "CBLC", "COLR", "CPAL",
  "SVG ", "sbix", "acnt", "avar", "bdat", "bloc", "bsln", "cvar", "fdsc",
  "feat", "fmtx", "fvar", "gvar", "hsty", "just", "lcar", "mort", "morx",
  "opbd", "prop", "trak", "Zapf", "Silf", "Glat", "Gloc", "Feat", "Sill",
];

// Name records tools/rename-subset-fonts.py rewrites. The renamed subset must
// name the renamed Audivane Latin family and carry no reserved name in any of
// them (the rewrite always produces both the words Audivane and Latin, even
// in forms like "2.3;IBM ;AudivaneMono Latin-Medium" that are neither the
// spaced family nor its PostScript form, so both words serve as the shared
// tokens below). tools/rename-subset-fonts.py reads this list and
// FONT_DIRECTORIES from here instead of keeping its own copies.
export const RENAMED_NAME_IDS = [1, 3, 4, 6, 16, 18, 21, 25];

// Name records every bundled subset must contain, so stripping them fails
// the check instead of skipping an optional per-record validation.
const REQUIRED_NAME_IDS = [0, 14];

// Per-family policy comes from third_party/fonts.json. A family with
// `reservedFontNames` (IBM Plex: "Plex") is a renamed modified subset: its
// primary names must carry the Audivane Latin family and never the reserved
// name, and its file differs from the upstream subset only in the name table.
// A family without reserved names either ships a pinned upstream fontsource
// subset unmodified (its bytes must equal the recorded original) or, with a
// `subset` record (Geist), our own subset of the upstream font under its own
// names.
//
// nameID -> validator; a validator returns an error message naming the
// record's content, or null when the record is intact. Records that are
// absent are only reported through REQUIRED_NAME_IDS.
const renamedValidations = (family, holder) => new Map([
  [0, (text) => (/copyright/i.test(text) && text.includes(holder)
    ? null
    : `nameID 0 no longer preserves the ${holder} copyright: ${text}`)],
  [1, (text) => (text.startsWith(family)
    ? null
    : `nameID 1 is not based on ${family}: ${text}`)],
  [6, (text) => (text.startsWith(family.replace(/ /g, ""))
    ? null
    : `PostScript name is not based on ${family.replace(/ /g, "")}: ${text}`)],
  [13, (text) => (/SIL Open Font License/i.test(text)
    ? null
    : `nameID 13 no longer carries the OFL license text: ${text}`)],
  [14, (text) => (/^https?:\/\/\S+$/.test(text)
    ? null
    : `nameID 14 is not the OFL license URL: ${text}`)],
  [16, (text) => (text.startsWith(family)
    ? null
    : `nameID 16 is not based on ${family}: ${text}`)],
]);

const upstreamValidations = (family, holder) => new Map([
  [0, (text) => (/copyright/i.test(text) && text.includes(holder)
    ? null
    : `nameID 0 does not carry the ${holder} copyright: ${text}`)],
  [1, (text) => (text.startsWith(family) ? null : `nameID 1 is not based on ${family}: ${text}`)],
  [16, (text) => (text === family ? null : `nameID 16 is not ${family}: ${text}`)],
  [14, (text) => (/^https?:\/\/\S+$/.test(text)
    ? null
    : `nameID 14 is not the OFL license URL: ${text}`)],
]);

export function parseWoff2(bytes, label = "font") {
  if (bytes.length < 48 || bytes.readUInt32BE(0) !== 0x774f4632) {
    throw new Error(`${label}: not a WOFF2 file`);
  }
  const numTables = bytes.readUInt16BE(12);
  const totalCompressedSize = bytes.readUInt32BE(20);
  const pos = { i: 48 };
  const readUIntBase128 = () => {
    let value = 0;
    for (let n = 0; n < 5; n += 1) {
      const byte = bytes[pos.i];
      pos.i += 1;
      if (n === 0 && byte === 0x80) throw new Error(`${label}: UIntBase128 with leading zero`);
      if (value & 0xfe000000) throw new Error(`${label}: UIntBase128 overflow`);
      value = ((value << 7) | (byte & 0x7f)) >>> 0;
      if ((byte & 0x80) === 0) return value;
    }
    throw new Error(`${label}: UIntBase128 longer than five bytes`);
  };
  const tables = [];
  for (let n = 0; n < numTables; n += 1) {
    const flags = bytes[pos.i];
    pos.i += 1;
    const tagIndex = flags & 0x3f;
    const transformVersion = (flags >> 6) & 0x03;
    let tag;
    if (tagIndex === 63) {
      tag = bytes.subarray(pos.i, pos.i + 4).toString("latin1");
      pos.i += 4;
    } else {
      tag = KNOWN_TAGS[tagIndex];
    }
    const transformed = tag === "glyf" || tag === "loca" ? transformVersion !== 3 : transformVersion !== 0;
    const origLength = readUIntBase128();
    const storedLength = transformed ? readUIntBase128() : origLength;
    tables.push({ tag, transformVersion, origLength, storedLength });
  }
  const compressedStart = pos.i;
  const compressedEnd = compressedStart + totalCompressedSize;
  if (compressedEnd > bytes.length) throw new Error(`${label}: compressed block runs past the file end`);
  const stream = brotliDecompressSync(bytes.subarray(compressedStart, compressedEnd));
  const expectedLength = tables.reduce((sum, table) => sum + table.storedLength, 0);
  if (stream.length !== expectedLength) {
    throw new Error(`${label}: decompressed stream is ${stream.length} bytes, the table directory describes ${expectedLength}`);
  }
  let offset = 0;
  for (const table of tables) {
    table.bytes = stream.subarray(offset, offset + table.storedLength);
    table.streamOffset = offset;
    offset += table.storedLength;
  }
  return { tables, stream, compressedStart, compressedEnd };
}

// Re-embed a mutated decompressed table stream into a copy of the file,
// keeping the table directory and updating the two size header fields.
export function recompressWoff2(bytes, parsed, stream) {
  const compressed = brotliCompressSync(stream);
  const output = Buffer.concat([
    bytes.subarray(0, parsed.compressedStart),
    compressed,
    bytes.subarray(parsed.compressedEnd),
  ]);
  output.writeUInt32BE(output.length, 8);
  output.writeUInt32BE(compressed.length, 20);
  return output;
}

export function nameRecords(parsed, label = "font") {
  const entry = parsed.tables.find((table) => table.tag === "name");
  if (!entry) throw new Error(`${label}: font has no name table`);
  const bytes = entry.bytes;
  const count = bytes.readUInt16BE(2);
  const stringOffset = bytes.readUInt16BE(4);
  const records = [];
  for (let n = 0; n < count; n += 1) {
    const base = 6 + n * 12;
    const platformID = bytes.readUInt16BE(base);
    const nameID = bytes.readUInt16BE(base + 6);
    const length = bytes.readUInt16BE(base + 8);
    const offset = bytes.readUInt16BE(base + 10);
    const raw = bytes.subarray(stringOffset + offset, stringOffset + offset + length);
    const text = platformID === 0 || platformID === 3
      ? Buffer.from(raw).swap16().toString("utf16le")
      : raw.toString("latin1");
    records.push({ platformID, nameID, text });
  }
  return records;
}

// Fingerprint every stored table except `name`, with the head table's
// checkSumAdjustment zeroed. Fields are hashed as fixed-width big-endian
// integers so no adjacent field or table can shift another's boundary.
export function contentFingerprint(tables) {
  const hash = createHash("sha256");
  const ordered = tables.filter((table) => table.tag !== "name").sort((a, b) => (a.tag < b.tag ? -1 : 1));
  const frame = Buffer.alloc(16);
  frame.writeUInt32BE(ordered.length, 0);
  hash.update(frame);
  for (const table of ordered) {
    hash.update(table.tag.padEnd(4, " "));
    frame.writeUInt32BE(table.transformVersion, 0);
    frame.writeUInt32BE(table.origLength, 4);
    frame.writeUInt32BE(table.storedLength, 8);
    frame.writeUInt32BE(table.bytes.length, 12);
    hash.update(frame);
    if (table.tag === "head") {
      const bytes = Buffer.from(table.bytes);
      bytes.fill(0, 8, 12); // checkSumAdjustment, required to change on any byte edit
      hash.update(bytes);
    } else {
      hash.update(table.bytes);
    }
  }
  return hash.digest("hex");
}

const hex64 = /^[0-9a-f]{64}$/;

function readFamilies(manifest) {
  if (!Array.isArray(manifest.families) || manifest.families.length === 0) {
    throw new Error(`${FONT_MANIFEST}: families must list each bundled family`);
  }
  const families = new Map();
  for (const entry of manifest.families) {
    const label = `${FONT_MANIFEST}: family ${entry?.family}`;
    if (!entry || typeof entry.family !== "string" || entry.family === "") throw new Error(`${FONT_MANIFEST}: family without a name`);
    if (families.has(entry.family)) throw new Error(`${label} is listed twice`);
    if (typeof entry.upstream !== "string" || !entry.upstream) throw new Error(`${label}: missing upstream`);
    if (typeof entry.copyrightHolder !== "string" || !entry.copyrightHolder) throw new Error(`${label}: missing copyrightHolder`);
    if (!Array.isArray(entry.reservedFontNames)) throw new Error(`${label}: reservedFontNames must be an array`);
    if (typeof entry.notice !== "string" || !/^[A-Za-z0-9._-]+\.txt$/.test(entry.notice)) {
      throw new Error(`${label}: notice must name a .txt file kept beside the family's fonts`);
    }
    const renamed = entry.reservedFontNames.length > 0;
    if (renamed && !(entry.family.includes("Audivane") && entry.family.includes("Latin"))) {
      throw new Error(`${label}: a family with a Reserved Font Name must be renamed to an Audivane Latin family`);
    }
    const subset = entry.subset ?? null;
    if (subset) {
      if (renamed) throw new Error(`${label}: a family with a Reserved Font Name cannot ship a subset under its own name`);
      if (!/^@?[a-z0-9/-]+@\d+\.\d+\.\d+$/.test(String(entry.source))) throw new Error(`${label}: source must pin the upstream package version`);
      if (typeof subset.from !== "string" || !subset.from) throw new Error(`${label}: subset.from must name the upstream file`);
      if (!hex64.test(String(subset.fromSha256))) throw new Error(`${label}: subset.fromSha256 is not a SHA-256 hex digest`);
      if (typeof subset.axes !== "string" || !subset.axes) throw new Error(`${label}: subset.axes must record the axis range`);
      if (typeof subset.unicodes !== "string" || !/^U\+/.test(subset.unicodes)) throw new Error(`${label}: subset.unicodes must list the code points`);
      if (typeof subset.command !== "string" || !subset.command.includes("pyftsubset")) throw new Error(`${label}: subset.command must record the pyftsubset call`);
    } else if (!renamed && !/^@fontsource\/[a-z0-9-]+@\d+\.\d+\.\d+$/.test(String(entry.source))) {
      throw new Error(`${label}: an unmodified family must pin its @fontsource package version`);
    }
    for (const reserved of entry.reservedFontNames) {
      if (entry.family.toLowerCase().includes(String(reserved).toLowerCase())) throw new Error(`${label}: family name uses the reserved name ${reserved}`);
    }
    families.set(entry.family, { ...entry, renamed, subset });
  }
  if (!families.get("Audivane Mono Latin")?.renamed) throw new Error(`${FONT_MANIFEST}: missing renamed family Audivane Mono Latin`);
  if (!families.get("Geist")?.subset) throw new Error(`${FONT_MANIFEST}: missing the Geist subset family`);
  return families;
}

export async function checkSubsetFonts({ root = ROOT, update = false } = {}) {
  const manifestPath = path.join(root, FONT_MANIFEST);
  const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
  if (manifest.license !== "OFL-1.1") throw new Error(`${FONT_MANIFEST}: license must be OFL-1.1`);
  if (!String(manifest.tool).startsWith("fonttools")) throw new Error(`${FONT_MANIFEST}: tool must pin fonttools`);
  const families = readFamilies(manifest);

  const onDisk = [];
  for (const directory of FONT_DIRECTORIES) {
    for (const name of readdirSync(path.join(root, directory)).sort()) {
      if (name.endsWith(".woff2")) onDisk.push(`${directory}/${name}`);
    }
  }
  const recorded = manifest.files.map((file) => file.path);
  const missing = onDisk.filter((relative) => !recorded.includes(relative));
  const stale = recorded.filter((relative) => !onDisk.includes(relative));
  if (missing.length > 0) throw new Error(`${FONT_MANIFEST}: bundled fonts missing from the manifest: ${missing.join(", ")}`);
  if (stale.length > 0) throw new Error(`${FONT_MANIFEST}: manifest lists fonts that are not bundled: ${stale.join(", ")}`);
  if (new Set(recorded).size !== recorded.length) throw new Error(`${FONT_MANIFEST}: duplicate manifest paths`);
  const unusedFamilies = [...families.keys()].filter((family) => !manifest.files.some((file) => file.family === family));
  if (unusedFamilies.length > 0) throw new Error(`${FONT_MANIFEST}: families without bundled files: ${unusedFamilies.join(", ")}`);

  const byOriginal = new Map();
  for (const file of manifest.files) {
    const label = file.path;
    const family = families.get(file.family);
    if (!family) throw new Error(`${label}: family ${file.family} is not listed in the manifest families`);
    if (!hex64.test(String(file.sha256))) throw new Error(`${label}: sha256 is not a SHA-256 hex digest`);
    if (family.subset) {
      if (file.upstreamFile !== family.subset.from) throw new Error(`${label}: upstreamFile must be the subset source ${family.subset.from}`);
    } else {
      if (!hex64.test(String(file.originalSha256))) throw new Error(`${label}: originalSha256 is not a SHA-256 hex digest`);
      if (!hex64.test(String(file.originalContentSha256))) {
        throw new Error(`${label}: originalContentSha256 is missing from the manifest`);
      }
    }
    if (!family.renamed && !family.subset && !(typeof file.upstreamFile === "string" && file.upstreamFile.startsWith("files/") && file.upstreamFile.endsWith(".woff2"))) {
      throw new Error(`${label}: upstreamFile must name the file inside ${family.source}`);
    }
    const bytes = readFileSync(path.join(root, file.path));
    const actualSha256 = createHash("sha256").update(bytes).digest("hex");
    if (actualSha256 !== file.sha256) throw new Error(`${label}: file bytes do not match the recorded sha256`);
    if (family.renamed && file.originalSha256 === file.sha256) throw new Error(`${label}: original and renamed hashes are identical; the rename was not applied`);
    if (!family.renamed && !family.subset && file.originalSha256 !== file.sha256) throw new Error(`${label}: unmodified upstream subset differs from its original bytes`);

    const parsed = parseWoff2(bytes, label);
    const actualFingerprint = contentFingerprint(parsed.tables);
    if (update) {
      file.contentSha256 = actualFingerprint;
    } else if (!hex64.test(String(file.contentSha256))) {
      throw new Error(`${label}: contentSha256 is missing from the manifest`);
    }
    if (actualFingerprint !== file.contentSha256) {
      throw new Error(`${label}: content fingerprint ${actualFingerprint} does not match the recorded contentSha256 ${file.contentSha256}; glyphs, metrics or other non-name font data changed`);
    }
    if (!family.subset && file.contentSha256 !== file.originalContentSha256) {
      throw new Error(`${label}: content fingerprint ${file.contentSha256} does not match the recorded upstream originalContentSha256 ${file.originalContentSha256}; glyphs, metrics or other non-name font data differ from the original subset`);
    }

    const records = nameRecords(parsed, label);
    for (const required of REQUIRED_NAME_IDS) {
      if (!records.some((record) => record.nameID === required)) {
        throw new Error(`${label}: no nameID ${required} record is present; the ${family.copyrightHolder} copyright or OFL license URL record was removed`);
      }
    }
    const validations = family.renamed
      ? renamedValidations(family.family, family.copyrightHolder)
      : upstreamValidations(family.family, family.copyrightHolder);
    for (const record of records) {
      for (const reserved of family.reservedFontNames) {
        if (record.text.toLowerCase().includes(String(reserved).toLowerCase())) {
          throw new Error(`${label}: nameID ${record.nameID} still contains the reserved name: ${record.text}`);
        }
      }
      if (family.renamed && RENAMED_NAME_IDS.includes(record.nameID)
        && !(record.text.includes("Audivane") && record.text.includes("Latin"))) {
        throw new Error(`${label}: nameID ${record.nameID} does not carry the renamed Audivane Latin family: ${record.text}`);
      }
      if (!family.renamed && /audivane/i.test(record.text)) {
        throw new Error(`${label}: unmodified upstream subset carries an Audivane name: ${record.text}`);
      }
      const validate = validations.get(record.nameID);
      if (validate) {
        const failure = validate(record.text);
        if (failure) throw new Error(`${label}: ${failure}`);
      }
    }

    if (family.subset) continue;
    const original = byOriginal.get(file.originalSha256);
    if (original) {
      if (
        original.sha256 !== file.sha256
        || original.contentSha256 !== file.contentSha256
        || original.originalContentSha256 !== file.originalContentSha256
        || original.family !== file.family
      ) {
        throw new Error(`${label}: same originalSha256 as ${original.path} but a different renamed file hash, content fingerprint or family`);
      }
    } else {
      byOriginal.set(file.originalSha256, file);
    }
  }

  // Every directory keeps the OFL notice of each family it bundles.
  for (const directory of FONT_DIRECTORIES) {
    const bundled = new Set(manifest.files
      .filter((file) => path.posix.dirname(file.path) === directory)
      .map((file) => file.family));
    for (const name of bundled) {
      const family = families.get(name);
      const notice = `${directory}/${family.notice}`;
      let text;
      try {
        text = readFileSync(path.join(root, notice), "utf8");
      } catch {
        throw new Error(`${notice}: the ${family.upstream} OFL notice is missing beside its fonts`);
      }
      if (!text.includes("SIL OPEN FONT LICENSE") || !text.includes(family.copyrightHolder)) {
        throw new Error(`${notice}: is not the ${family.upstream} OFL notice`);
      }
      for (const reserved of family.reservedFontNames) {
        if (!text.includes(`Reserved Font Name "${reserved}"`)) {
          throw new Error(`${notice}: does not declare the Reserved Font Name ${reserved}`);
        }
      }
    }
  }

  if (update) {
    writeFileSync(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`);
  }
  return { files: manifest.files.length, families: families.size, updated: update };
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const args = process.argv.slice(2);
  if (args[0] === "--fingerprint") {
    for (const file of args.slice(1)) {
      console.log(`${contentFingerprint(parseWoff2(readFileSync(file), file).tables)}  ${file}`);
    }
  } else {
    const update = args.includes("--update");
    const summary = await checkSubsetFonts({ update });
    const note = update ? " (contentSha256 re-recorded from disk)" : "";
    console.log(`[fonts] ${summary.files} bundled subsets in ${summary.families} families match ${FONT_MANIFEST}: hashes, renamed or upstream names, preserved copyright and OFL records, recorded glyph/metric data${note}`);
  }
}
