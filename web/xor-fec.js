// Bounded canonical GLD1/GLP1 parsing and grouped XOR reconstruction.
export const LIMITS = Object.freeze({
  headerBytes: 20,
  maxDatagramBytes: 1020,
  maxAccessUnitBytes: 1048576,
  minStride: 256,
  maxStride: 1000,
  groupSize: 8,
  partialAccessUnits: 2,
  ttlMs: 250,
  retiredSequences: 8,
  reorderSequences: 8,
  // Per row: AU<=1MiB, seen<=4096, parity<=AU/8+1000, groups<=512.
  maxKnownRetainedTypedArrayBytes: 2 * (1048576 + 4096 + 132072 + 512),
});
export const MAGIC = Object.freeze({ data: 0x31444c47, parity: 0x31504c47 }); // GLD1, GLP1, little endian
const check = (ok, message) => {
  if (!ok) throw new Error(message);
};
const u32 = (value) =>
  Number.isInteger(value) && value >= 0 && value <= 0xffffffff;
const newer = (a, b) => a !== b && (a - b) >>> 0 < 0x80000000;
const equal = (a, b) =>
  a.length === b.length && a.every((value, index) => value === b[index]);
const size = (row, index) =>
  Math.min(row.stride, row.total - index * row.stride);
const groupSize = (row, first) => Math.min(LIMITS.groupSize, row.count - first);

export function parseDatagram(bytes) {
  check(
    bytes instanceof Uint8Array &&
      bytes.byteLength >= LIMITS.headerBytes &&
      bytes.byteLength <= LIMITS.maxDatagramBytes,
    "bounded datagram",
  );
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength),
    magic = view.getUint32(0, true);
  const kind =
    magic === MAGIC.data ? "data" : magic === MAGIC.parity ? "parity" : null;
  check(kind, "GLD1 or GLP1 magic");
  const row = {
    kind,
    sequence: view.getUint32(4, true),
    total: view.getUint32(8, true),
    index: view.getUint16(12, true),
    count: view.getUint16(14, true),
    stride: view.getUint16(16, true),
  };
  check(
    row.total >= 24 &&
      row.total <= LIMITS.maxAccessUnitBytes &&
      row.stride >= LIMITS.minStride &&
      row.stride <= LIMITS.maxStride &&
      row.count === Math.ceil(row.total / row.stride) &&
      row.index < row.count,
    "canonical fragment geometry",
  );
  if (kind === "data")
    check(view.getUint16(18, true) === 0, "GLD1 reserved bytes");
  else
    check(
      row.index % LIMITS.groupSize === 0 &&
        view.getUint8(18) === groupSize(row, row.index) &&
        view.getUint8(19) === 0,
      "GLP1 canonical group",
    );
  check(
    bytes.byteLength === LIMITS.headerBytes + size(row, row.index),
    "exact canonical payload extent",
  );
  return { ...row, body: bytes.subarray(LIMITS.headerBytes) };
}

function header(kind, row, index) {
  const output = new Uint8Array(LIMITS.headerBytes + size(row, index)),
    view = new DataView(output.buffer);
  view.setUint32(0, MAGIC[kind], true);
  view.setUint32(4, row.sequence, true);
  view.setUint32(8, row.total, true);
  view.setUint16(12, index, true);
  view.setUint16(14, row.count, true);
  view.setUint16(16, row.stride, true);
  if (kind === "parity") view.setUint8(18, groupSize(row, index));
  return output;
}

export function parityForGroup(datagrams) {
  check(
    Array.isArray(datagrams) &&
      datagrams.length > 0 &&
      datagrams.length <= LIMITS.groupSize,
    "one full group of actual data fragments",
  );
  const rows = datagrams.map(parseDatagram);
  check(
    rows.every((row) => row.kind === "data"),
    "parity input is canonical GLD1",
  );
  const first = Math.min(...rows.map((row) => row.index)),
    base = rows[0];
  check(
    first % LIMITS.groupSize === 0 &&
      rows.length === groupSize(base, first) &&
      new Set(rows.map((row) => row.index)).size === rows.length &&
      rows.every(
        (row) =>
          row.sequence === base.sequence &&
          row.total === base.total &&
          row.count === base.count &&
          row.stride === base.stride &&
          row.index >= first &&
          row.index < first + rows.length,
      ),
    "exact complete group with one geometry",
  );
  const output = header("parity", base, first),
    body = output.subarray(LIMITS.headerBytes);
  for (const row of rows)
    for (let index = 0; index < row.body.length; index++)
      body[index] ^= row.body[index];
  return output;
}

const COUNTERS = [
  "received_datagrams",
  "received_bytes",
  "data_datagrams",
  "data_bytes",
  "parity_datagrams",
  "parity_bytes",
  "forwarded_original_fragments",
  "forwarded_original_bytes",
  "recovered_fragments",
  "recovered_payload_bytes",
  "recovered_application_bytes",
  "completed_access_units",
  "duplicate_data",
  "duplicate_recovered_data",
  "duplicate_parity",
  "conflicting_data",
  "conflicting_parity",
  "geometry_conflicts",
  "malformed_datagrams",
  "expired_access_units",
  "evicted_access_units",
  "horizon_access_units",
  "closed_access_units",
  "stale_datagrams",
  "retired_datagrams",
  "redundant_parity",
  "typed_array_bytes_allocated_total",
];

export class XorFecReceiver {
  constructor() {
    this.partial = new Map();
    this.retired = new Map();
    this.highest = null;
    this.time = 0;
    this.closed = false;
    this.stats = Object.fromEntries(COUNTERS.map((name) => [name, 0]));
    this.countersSaturated = false;
    this.knownRetainedPeak = 0;
    this.frameRecordsPeak = 0;
  }
  _inc(name, amount = 1) {
    if (this.stats[name] > Number.MAX_SAFE_INTEGER - amount) {
      this.stats[name] = Number.MAX_SAFE_INTEGER;
      this.countersSaturated = true;
    } else this.stats[name] += amount;
  }
  _time(now) {
    check(
      !this.closed && Number.isFinite(now) && now >= this.time,
      "live monotonic local milliseconds",
    );
    this.time = now;
  }
  knownBytes() {
    let total = 0;
    for (const row of this.partial.values())
      total +=
        row.data.byteLength +
        row.seen.byteLength +
        row.parity.byteLength +
        row.paritySeen.byteLength;
    return total;
  }
  _memory() {
    const bytes = this.knownBytes();
    check(
      bytes <= LIMITS.maxKnownRetainedTypedArrayBytes,
      "reachable row array bound",
    );
    this.knownRetainedPeak = Math.max(this.knownRetainedPeak, bytes);
    this.frameRecordsPeak = Math.max(this.frameRecordsPeak, this.partial.size);
  }
  _retire(sequence, reason, losses) {
    const row = this.partial.get(sequence);
    this.partial.delete(sequence);
    this.retired.delete(sequence);
    while (this.retired.size >= LIMITS.retiredSequences)
      this.retired.delete(this.retired.keys().next().value);
    this.retired.set(sequence, reason);
    if (reason !== "complete" && row) {
      const missing = row.count - row.received;
      losses.push({
        sequence,
        reason,
        missing_fragments: missing,
        total_fragments: row.count,
      });
      const counter = {
        expired: "expired_access_units",
        evicted: "evicted_access_units",
        horizon: "horizon_access_units",
        closed: "closed_access_units",
      }[reason];
      if (counter) this._inc(counter);
    }
  }
  retire(sequence, reason = "downstream-retired") {
    check(
      !this.closed && u32(sequence),
      "live bounded FEC retirement sequence",
    );
    if (!this.retired.has(sequence)) this._retire(sequence, reason, []);
  }
  partialSequences() {
    check(!this.closed, "live FEC receiver");
    return Array.from(this.partial.keys());
  }
  _expire(now, losses) {
    for (const row of this.partial.values())
      if (now - row.started >= LIMITS.ttlMs)
        this._retire(row.sequence, "expired", losses);
  }
  expire(now) {
    this._time(now);
    const losses = [];
    this._expire(now, losses);
    return losses;
  }
  _row(packet, now, losses) {
    if (this.retired.has(packet.sequence)) {
      if (
        packet.kind === "parity" &&
        this.retired.get(packet.sequence) === "complete"
      )
        this._inc("redundant_parity");
      else this._inc("retired_datagrams");
      return null;
    }
    if (
      this.highest !== null &&
      packet.sequence !== this.highest &&
      !newer(packet.sequence, this.highest) &&
      (this.highest - packet.sequence) >>> 0 > LIMITS.reorderSequences
    ) {
      this._inc("stale_datagrams");
      return null;
    }
    if (this.highest === null || newer(packet.sequence, this.highest)) {
      this.highest = packet.sequence;
      for (const row of this.partial.values())
        if (
          row.sequence !== this.highest &&
          !newer(row.sequence, this.highest) &&
          (this.highest - row.sequence) >>> 0 > LIMITS.reorderSequences
        )
          this._retire(row.sequence, "horizon", losses);
    }
    let row = this.partial.get(packet.sequence);
    if (row) {
      if (
        row.total !== packet.total ||
        row.count !== packet.count ||
        row.stride !== packet.stride
      ) {
        this._inc("geometry_conflicts");
        this._retire(row.sequence, "geometry-conflict", losses);
        return null;
      }
      return row;
    }
    if (this.partial.size === LIMITS.partialAccessUnits) {
      const oldest = Array.from(this.partial.keys()).reduce((a, b) =>
        newer(a, b) ? b : a,
      );
      if (!newer(packet.sequence, oldest)) {
        this._inc("stale_datagrams");
        return null;
      }
      this._retire(oldest, "evicted", losses);
    }
    const groups = Math.ceil(packet.count / LIMITS.groupSize),
      allocation =
        packet.total + packet.count + groups * packet.stride + groups;
    check(
      this.knownBytes() + allocation <= LIMITS.maxKnownRetainedTypedArrayBytes,
      "reserve bounded reachable row arrays before allocation",
    );
    row = {
      ...packet,
      body: undefined,
      started: now,
      received: 0,
      data: new Uint8Array(packet.total),
      seen: new Uint8Array(packet.count),
      parity: new Uint8Array(groups * packet.stride),
      paritySeen: new Uint8Array(groups),
    };
    this.partial.set(row.sequence, row);
    this._inc("typed_array_bytes_allocated_total", allocation);
    this._memory();
    return row;
  }
  _recover(row, first) {
    const group = first / LIMITS.groupSize;
    if (!row.paritySeen[group]) return { recovered: null, conflict: false };
    const count = groupSize(row, first);
    let missing = -1,
      absent = 0;
    for (let index = first; index < first + count; index++)
      if (!row.seen[index]) {
        missing = index;
        absent++;
      }
    if (absent > 1) return { recovered: null, conflict: false };
    const work = row.parity.slice(
      group * row.stride,
      group * row.stride + size(row, first),
    );
    this._inc("typed_array_bytes_allocated_total", work.byteLength);
    for (let index = first; index < first + count; index++)
      if (row.seen[index]) {
        const length = size(row, index),
          offset = index * row.stride;
        for (let at = 0; at < length; at++) work[at] ^= row.data[offset + at];
      }
    if (absent === 0)
      return { recovered: null, conflict: work.some((value) => value !== 0) };
    const length = size(row, missing);
    if (work.subarray(length).some((value) => value !== 0))
      return { recovered: null, conflict: true };
    row.data.set(work.subarray(0, length), missing * row.stride);
    row.seen[missing] = 2;
    row.received++;
    const bytes = header("data", row, missing);
    bytes.set(work.subarray(0, length), LIMITS.headerBytes);
    this._inc("typed_array_bytes_allocated_total", bytes.byteLength);
    this._inc("recovered_fragments");
    this._inc("recovered_payload_bytes", length);
    this._inc("recovered_application_bytes", bytes.byteLength);
    return { recovered: { bytes, recovered: true }, conflict: false };
  }
  receive(bytes, now) {
    this._time(now);
    const losses = [];
    this._expire(now, losses);
    this._inc("received_datagrams");
    if (bytes instanceof Uint8Array)
      this._inc("received_bytes", bytes.byteLength);
    let packet;
    try {
      packet = parseDatagram(bytes);
    } catch {
      this._inc("malformed_datagrams");
      return {
        forward: [],
        losses: [...losses, { sequence: null, reason: "malformed" }],
        status: "malformed",
      };
    }
    this._inc(packet.kind === "data" ? "data_datagrams" : "parity_datagrams");
    this._inc(
      packet.kind === "data" ? "data_bytes" : "parity_bytes",
      bytes.byteLength,
    );
    const row = this._row(packet, now, losses);
    if (!row)
      return {
        forward: [],
        losses,
        status: losses.some((loss) => loss.reason === "geometry-conflict")
          ? "conflict"
          : "retired-or-stale",
      };
    const first =
        Math.floor(packet.index / LIMITS.groupSize) * LIMITS.groupSize,
      group = first / LIMITS.groupSize;
    if (packet.kind === "data") {
      const offset = packet.index * row.stride;
      if (row.seen[packet.index]) {
        if (
          !equal(
            packet.body,
            row.data.subarray(offset, offset + packet.body.length),
          )
        ) {
          this._inc("conflicting_data");
          this._retire(row.sequence, "data-conflict", losses);
          return { forward: [], losses, status: "conflict" };
        }
        this._inc(
          row.seen[packet.index] === 2
            ? "duplicate_recovered_data"
            : "duplicate_data",
        );
        return { forward: [], losses, status: "duplicate" };
      }
      row.data.set(packet.body, offset);
      row.seen[packet.index] = 1;
      row.received++;
    } else {
      const offset = group * row.stride;
      if (row.paritySeen[group]) {
        if (
          !equal(
            packet.body,
            row.parity.subarray(offset, offset + packet.body.length),
          )
        ) {
          this._inc("conflicting_parity");
          this._retire(row.sequence, "parity-conflict", losses);
          return { forward: [], losses, status: "conflict" };
        }
        this._inc("duplicate_parity");
        return { forward: [], losses, status: "duplicate" };
      }
      row.parity.set(packet.body, offset);
      row.paritySeen[group] = 1;
    }
    const result = this._recover(row, first);
    if (result.conflict) {
      this._inc("conflicting_parity");
      this._retire(row.sequence, "parity-conflict", losses);
      return { forward: [], losses, status: "conflict" };
    }
    const forward = [];
    if (packet.kind === "data") {
      forward.push({ bytes, recovered: false });
      this._inc("forwarded_original_fragments");
      this._inc("forwarded_original_bytes", bytes.byteLength);
    }
    if (result.recovered) forward.push(result.recovered);
    if (row.received === row.count) {
      this._inc("completed_access_units");
      this._retire(row.sequence, "complete", losses);
    }
    return {
      forward,
      losses,
      status: forward.length ? "forwarded" : "parity-only",
    };
  }
  snapshot() {
    return {
      schema: "gliss-grouped-xor-v1",
      limits: LIMITS,
      closed: this.closed,
      counters: { ...this.stats },
      counters_saturated: this.countersSaturated,
      gauges: {
        partial_access_units: this.partial.size,
        retired_sequences: this.retired.size,
        known_retained_typed_array_bytes: this.knownBytes(),
        known_retained_typed_array_bytes_peak: this.knownRetainedPeak,
        live_typed_arrays: 4 * this.partial.size,
        partial_access_units_peak: this.frameRecordsPeak,
      },
      memory_scope:
        "Known reachable row typed-array byte lengths only. Excludes JS object/Map/allocator/GC overhead, input/output ownership and <=1000-byte XOR scratch; not complete heap or RSS.",
    };
  }
  close() {
    check(!this.closed, "one close");
    const losses = [];
    for (const row of this.partial.values())
      this._retire(row.sequence, "closed", losses);
    this.retired.clear();
    this.highest = null;
    this.time = 0;
    this.closed = true;
    return losses;
  }
}
