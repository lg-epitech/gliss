import { PredictiveReceiver, newer } from "./predictive-state.js";
import { XorFecReceiver, parseDatagram } from "./xor-fec.js";
const require = (ok, message) => {
  if (!ok) throw new Error(message);
};

// Joins grouped XOR retirement to the unchanged predictive reference gate.
export class FecPredictiveReceiver {
  constructor() {
    this.fec = new XorFecReceiver();
    this.inner = new PredictiveReceiver({
      onAssemblyRetire: (sequence) => this.fec.retire(sequence),
    });
    this.closed = false;
    this.floor = null;
    this.highest = null;
    this.generation = 0;
    this.stats = {
      receivedDataDatagrams: 0,
      receivedDataBytes: 0,
      receivedParityDatagrams: 0,
      receivedParityBytes: 0,
      forwardedDataDatagrams: 0,
      forwardedDataBytes: 0,
      recoveredFragments: 0,
      acceptedAccessUnitBytes: 0,
      retiredArrivals: 0,
      assemblyArrayAllocatedBytes: 0,
      gateArrayAllocatedBytes: 0,
      knownRetainedBytesPeak: 0,
    };
  }
  _live() {
    require(!this.closed, "closed FEC predictive receiver");
  }
  get acceptedBytes() {
    return this.inner.acceptedBytes;
  }
  _sequences() {
    return new Set([
      ...this.inner.partialSequences(),
      ...this.fec.partialSequences(),
    ]);
  }
  _retire(sequence) {
    this.inner.retire(sequence);
  }
  _sync() {
    const reference = this.inner.snapshot().reference;
    if (this.generation !== reference.generation) {
      this.generation = reference.generation;
      this.floor = this.highest;
      for (const sequence of this._sequences()) this._retire(sequence);
    }
    if (reference.lastDecodedSequence !== null)
      for (const sequence of this._sequences())
        if (!newer(sequence, reference.lastDecodedSequence))
          this._retire(sequence);
    this.stats.knownRetainedBytesPeak = Math.max(
      this.stats.knownRetainedBytesPeak,
      this.retainedBytes(),
    );
  }
  _loss(sequence, now) {
    if (sequence === null)
      for (const value of this._sequences()) this._retire(value);
    else this._retire(sequence);
    this.inner.loss(sequence, now);
    this._sync();
  }
  _expire(now) {
    for (const loss of this.fec.expire(now)) this._loss(loss.sequence, now);
    for (const sequence of this.inner.expireAssembly(now))
      this._loss(sequence, now);
    this._sync();
  }
  receive(bytes, now) {
    this._live();
    this._expire(now);
    let packet = null;
    try {
      packet = parseDatagram(bytes);
    } catch {}
    if (packet) {
      const prefix = packet.kind === "data" ? "receivedData" : "receivedParity";
      this.stats[`${prefix}Datagrams`]++;
      this.stats[`${prefix}Bytes`] += bytes.byteLength;
      if (this.floor !== null && !newer(packet.sequence, this.floor)) {
        this.stats.retiredArrivals++;
        return false;
      }
      if (this.highest === null || newer(packet.sequence, this.highest))
        this.highest = packet.sequence;
    }
    const generation = this.generation,
      result = this.fec.receive(bytes, now);
    // Retire dependencies before consuming any forward output from this call.
    for (const loss of result.losses) this._loss(loss.sequence, now);
    if (generation !== this.generation) return false;
    let admitted = false;
    for (const row of result.forward) {
      const part = parseDatagram(row.bytes);
      if (this.floor !== null && !newer(part.sequence, this.floor)) continue;
      this.stats.forwardedDataDatagrams++;
      this.stats.forwardedDataBytes += row.bytes.byteLength;
      if (row.recovered) this.stats.recoveredFragments++;
      const existed = this.inner.hasPartial(part.sequence),
        out = this.inner.pushFragment(row.bytes, now);
      if (!existed && (out.buffer || this.inner.hasPartial(part.sequence)))
        this.stats.assemblyArrayAllocatedBytes += part.total + part.count;
      for (const loss of out.losses) this._loss(loss, now);
      if (out.buffer && generation === this.generation) {
        this.stats.gateArrayAllocatedBytes += out.buffer.byteLength;
        const before = this.inner.acceptedBytes,
          accepted = this.inner.offerAccessUnit(out.buffer, now);
        this.stats.acceptedAccessUnitBytes += this.inner.acceptedBytes - before;
        admitted = accepted || admitted;
      }
      this._sync();
    }
    return admitted;
  }
  next(now) {
    this._live();
    this._expire(now);
    const next = this.inner.next(now);
    this._sync();
    return next;
  }
  complete(token, ok, now) {
    this._live();
    const result = this.inner.complete(token, ok, now);
    this._sync();
    return result;
  }
  loss(sequence, now) {
    this._live();
    this._loss(sequence, now);
  }
  observeHead(sequence, now) {
    this._live();
    this.inner.observeHead(sequence, now);
    if (this.highest === null || newer(sequence, this.highest))
      this.highest = sequence;
  }
  recovery(now) {
    this._live();
    this._expire(now);
    const result = this.inner.recovery(now);
    this._sync();
    return result;
  }
  retainedBytes() {
    return this.inner.retainedBytes() + this.fec.knownBytes();
  }
  snapshot() {
    return {
      ...this.inner.snapshot(),
      fec: this.fec.snapshot(),
      stats: { ...this.stats },
      lifecycle: {
        closed: this.closed,
        floor: this.floor,
        highest: this.highest,
        generation: this.generation,
      },
      memory: {
        reachableTypedArrayBytes: this.retainedBytes(),
        scope:
          "Known reachable receiver typed-array byte lengths only; excludes input ownership, JS objects, Maps, allocator and GC overhead.",
      },
    };
  }
  close() {
    this._live();
    this.fec.close();
    this.inner.close();
    this.floor = null;
    this.highest = null;
    this.generation = 0;
    this.closed = true;
  }
}
