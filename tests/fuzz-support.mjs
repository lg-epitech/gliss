// Deterministic fuzzing for the Node suite, on the scheme of
// test-support/rust/fuzz.rs: inputs come from a grammar, are changed byte by
// byte and checked against a reference, and each iteration has its own
// generator so that a failure replays alone from the seed and the iteration
// number it prints. docs/development.md describes the environment variables.
import test from 'node:test';
import {existsSync, readFileSync} from 'node:fs';
import {isMainThread, parentPort, Worker, workerData} from 'node:worker_threads';

// Mulberry32: 32 bits of state, which is what a JavaScript number adds exactly.
export class Rng {
  constructor(seed) { this.state = seed >>> 0; }
  next() {
    let word = this.state = (this.state + 0x6d2b79f5) >>> 0;
    word = Math.imul(word ^ (word >>> 15), word | 1);
    word ^= word + Math.imul(word ^ (word >>> 7), word | 61);
    return (word ^ (word >>> 14)) >>> 0;
  }
  below(bound) { return this.next() % bound; }
  between(low, high) { return low + this.below(high - low + 1); }
  chance(oneIn) { return this.below(oneIn) === 0; }
  byte() { return this.next() & 255; }
  bytes(length) { return Uint8Array.from({length}, () => this.byte()); }
  pick(items) { return items[this.below(items.length)]; }
}

// Values at the edges of what a length, a count or a size can hold.
const EXTREMES = [0, 1, 2, 0x7f, 0x80, 0xff, 0x100, 0x7fff, 0x8000, 0xffff, 0x10000, 0x100001,
  0x7fffffff, 0x80000000, 0xfffffffe, 0xffffffff];

// Returns `bytes` with one change: a flipped bit, a replaced, inserted or deleted run, a cut, a
// run copied from elsewhere, or an extreme value over a field. `fields` are offsets worth aiming at.
export function mutate(rng, bytes, fields = [], bigEndian = false) {
  const result = Array.from(bytes);
  if (!result.length) return rng.bytes(1 + rng.below(8));
  const at = fields.length && !rng.chance(4)
    ? Math.min(rng.pick(fields) + rng.below(4), result.length - 1) : rng.below(result.length);
  switch (rng.below(10)) {
    case 0: result[at] ^= 1 << rng.below(8); break;
    case 1: result[at] = rng.byte(); break;
    case 2: result[at] = rng.pick([0, 1, 0x7f, 0x80, 0xfe, 0xff]); break;
    case 3: result.splice(at, 0, ...rng.bytes(1 + rng.below(8))); break;
    case 4: result.splice(at, 1 + rng.below(8)); break;
    case 5: result.length = at; break;
    case 6: {
      const from = rng.below(result.length), run = result.slice(from, from + 1 + rng.below(32));
      result.splice(at, rng.chance(2) ? 0 : run.length, ...run);
      break;
    }
    case 7: case 8: {
      const value = rng.pick(EXTREMES), width = rng.chance(2) ? 2 : 4, field = [];
      for (let index = 0; index < width; index++) field.push((value >>> (8 * index)) & 255);
      if (bigEndian) field.reverse();
      result.splice(at, width, ...field);
      break;
    }
    default: result[at] = (result[at] + (rng.chance(2) ? 1 : 255)) & 255;
  }
  return Uint8Array.from(result);
}

function variable(name) {
  const text = process.env[name];
  if (text === undefined) return undefined;
  const value = Number(text);
  if (!Number.isSafeInteger(value) || value < 0) throw new Error(`${name} must be a number, not ${text}`);
  return value;
}

// FNV-1a over a name, so that each target has its own default seed.
function seedOf(name) {
  let hash = 0x811c9dc5;
  for (const character of name) hash = Math.imul(hash ^ character.codePointAt(0), 0x01000193);
  return hash >>> 0;
}

// How long one iteration may take before the run is declared stuck.
const STUCK_MS = 120000;

// Runs one target in a worker, which is the only way to stop JavaScript that does not return:
// a parser that loops for ever fails with its seed instead of hanging the suite.
function supervise(url, name, iterations) {
  // A campaign's seed is a 64-bit number on the Rust side; here its low 32 bits count.
  const seed = (variable('GLISS_FUZZ_SEED') ?? seedOf(name)) % 2 ** 32;
  const start = variable('GLISS_FUZZ_START') ?? 0, count = variable('GLISS_FUZZ_ITERATIONS') ?? iterations;
  return new Promise((resolve, reject) => {
    const worker = new Worker(new URL(url), {workerData: {fuzz: name, seed, start, count}});
    const began = Date.now();
    let iteration = start, progressed = began;
    const replay = () => `seed ${seed} iteration ${iteration}; replay it alone with ` +
      `GLISS_FUZZ_SEED=${seed} GLISS_FUZZ_START=${iteration} GLISS_FUZZ_ITERATIONS=1`;
    const watchdog = setInterval(() => {
      if (Date.now() - progressed < STUCK_MS) return;
      clearInterval(watchdog);
      void worker.terminate();
      reject(new Error(`fuzz target ${name} did not finish ${replay()}`));
    }, 1000);
    worker.on('message', number => { iteration = number; progressed = Date.now(); });
    worker.on('error', error => {
      clearInterval(watchdog);
      reject(new Error(`fuzz target ${name} failed at ${replay()}`, {cause: error}));
    });
    worker.on('exit', code => {
      clearInterval(watchdog);
      console.error(`fuzz target ${name}: ${count} iterations from ${start} with seed ${seed} in ${((Date.now() - began) / 1000).toFixed(1)}s`);
      if (code === 0) resolve(); else reject(new Error(`fuzz target ${name} ended with code ${code} at ${replay()}`));
    });
  });
}

// Declares the fuzz targets of the test file at `url`: {name: {iterations, run(rng, iteration)}}.
// `iterations` keeps the suite fast; GLISS_FUZZ_ITERATIONS replaces it for a campaign,
// GLISS_FUZZ_SEED selects another sequence of inputs and GLISS_FUZZ_START the first iteration.
// The file is loaded again in a worker for each target, so its other tests go under `onMain`.
export function fuzz(url, targets) {
  if (isMainThread) {
    for (const [name, target] of Object.entries(targets)) test(`fuzz: ${name}`, () => supervise(url, name, target.iterations));
    return;
  }
  const {fuzz: name, seed, start, count} = workerData;
  void (async () => {
    for (let iteration = start; iteration < start + count; iteration++) {
      parentPort.postMessage(iteration);
      // The golden ratio spreads consecutive iteration numbers over the seeds.
      await targets[name].run(new Rng(seed ^ Math.imul(iteration, 0x9e3779b9)), iteration);
    }
  })();
}

export const onMain = isMainThread;

export function hex(bytes) { return Array.from(bytes, byte => byte.toString(16).padStart(2, '0')).join(''); }
export function unhex(text) { return Uint8Array.from(text.match(/../g) ?? [], pair => parseInt(pair, 16)); }

// FNV-1a over 64 bits as sixteen hex digits, the fingerprint the Rust side writes for a long
// result. The hash is kept as two halves: its product does not fit a JavaScript number.
export function fingerprint(bytes) {
  let high = 0xcbf29ce4, low = 0x84222325;
  for (const byte of bytes) {
    low = (low ^ byte) >>> 0;
    // Times 2^40 + 0x1b3.
    const product = low * 0x1b3;
    high = (high * 0x1b3 + Math.floor(product / 2 ** 32) + low * 256) % 2 ** 32;
    low = product % 2 ** 32;
  }
  return high.toString(16).padStart(8, '0') + low.toString(16).padStart(8, '0');
}

// The lines of a vector file: the one in test-support/fuzz that the Rust tests keep current,
// then the one a Rust campaign wrote into the directory GLISS_FUZZ_VECTORS names, if any.
export function vectors(name) {
  const files = [new URL(`../test-support/fuzz/${name}`, import.meta.url)];
  const campaign = process.env.GLISS_FUZZ_VECTORS;
  if (campaign && existsSync(`${campaign}/${name}`)) files.push(`${campaign}/${name}`);
  return files.flatMap(file => readFileSync(file, 'utf8').split('\n').filter(Boolean));
}
