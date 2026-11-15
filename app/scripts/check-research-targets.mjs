#!/usr/bin/env node
// Usage: node scripts/check-research-targets.mjs [directory]
// No app dependencies, network access, or working-directory assumptions.
import { readdir, readFile } from 'node:fs/promises';
import { basename, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const directory = process.argv[2]
  ? resolve(process.argv[2])
  : fileURLToPath(new URL('../src/assets/research-targets/', import.meta.url));
const keys = [
  'id', 'name', 'kind', 'category', 'source', 'url', 'license', 'citation',
  'derived', 'points',
];
const stringKeys = ['id', 'name', 'source', 'url', 'license', 'citation'];

function validate(data, filename, ids) {
  const require = (condition, message) => {
    if (!condition) throw new Error(message);
  };
  require(data !== null && typeof data === 'object' && !Array.isArray(data), 'expected an object');
  require(keys.every(key => Object.hasOwn(data, key)), 'missing required schema field');
  require(Object.keys(data).every(key => keys.includes(key)), 'unknown schema field');
  for (const key of stringKeys) {
    require(typeof data[key] === 'string' && data[key].trim().length > 0, `${key}: expected a nonempty string`);
  }
  require(/^[a-z0-9]+(?:-[a-z0-9]+)*$/.test(data.id), 'id: expected a lowercase slug');
  require(basename(filename, '.json') === data.id, 'id must match the filename');
  require(!ids.has(data.id), `duplicate id: ${data.id}`);
  require(['target', 'bounds'].includes(data.kind), 'kind: expected target or bounds');
  require(['iem', 'headphone', 'speaker'].includes(data.category), 'category: expected iem, headphone, or speaker');
  require(typeof data.derived === 'boolean', 'derived: expected a boolean');
  require(Array.isArray(data.points) && data.points.length >= 2, 'points: expected at least two rows');

  const columns = data.kind === 'bounds' ? 3 : 2;
  let previous = 0;
  let reference;
  for (const [index, row] of data.points.entries()) {
    require(Array.isArray(row) && row.length === columns, `points[${index}]: expected ${columns} columns`);
    require(row.every(value => typeof value === 'number' && Number.isFinite(value)), `points[${index}]: expected finite numbers`);
    const [frequency, lower, upper] = row;
    require(frequency >= 20 && frequency <= 20000, `points[${index}]: frequency outside 20–20000 Hz`);
    require(frequency > previous, `points[${index}]: frequencies must strictly increase`);
    if (previous > 0) {
      require(Math.log2(frequency / previous) <= 1 / 24 + 1e-6, `points[${index}]: gap exceeds 1/24 octave`);
    }
    if (data.kind === 'bounds') {
      require(lower <= upper, `points[${index}]: lower exceeds upper`);
    }
    if (frequency === 1000) reference = row.slice(1);
    previous = frequency;
  }
  require(data.points[0][0] === 20 && previous === 20000, 'points must cover exactly 20–20000 Hz');
  // Require an explicit anchor, so interpolation conventions cannot hide an offset.
  require(reference !== undefined, 'points must include an explicit 1000 Hz row');
  require(reference.every(value => Math.abs(value) <= 0.05), '1000 Hz values must be 0 dB within 0.05 dB');
  ids.add(data.id);
}

try {
  const files = (await readdir(directory)).filter(file => file.endsWith('.json')).sort();
  if (files.length === 0) throw new Error(`no JSON files in ${directory}`);
  const ids = new Set();
  let failures = 0;
  let pointCount = 0;
  for (const file of files) {
    try {
      const data = JSON.parse(await readFile(resolve(directory, file), 'utf8'));
      validate(data, file, ids);
      pointCount += data.points.length;
    } catch (error) {
      console.error(`${file}: ${error.message}`);
      failures += 1;
    }
  }
  if (failures > 0) {
    process.exitCode = 1;
  } else {
    console.log(`Validated ${files.length} research curves (${pointCount} points): schema, frequency grid, bounds, and 1 kHz normalization.`);
  }
} catch (error) {
  console.error(error.message);
  process.exitCode = 1;
}
