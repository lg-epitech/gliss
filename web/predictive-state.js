// Predictive protocol state. Sockets, timers and WebCodecs remain at the
// transport and decoder boundaries.
import {parseVideoPacket} from './video-decoder.js';
const require=(ok,message)=>{if(!ok)throw new Error(message);};
const u32=value=>Number.isInteger(value)&&value>=0&&value<=0xffffffff;
export const newer=(a,b)=>a!==b&&((a-b)>>>0)<0x80000000;
export const LIMITS=Object.freeze({frameBytes:1048576,partialFrames:2,pendingFrames:4,pendingBytes:1048576,
  reorderFrames:8,reorderMs:8,maxAgeMs:250,recoveryIntervalMs:1000});
const same=(a,b)=>a.length===b.length&&a.every((value,index)=>value===b[index]);

export class PredictiveGate{
  constructor(){
    this.pending=new Map();this.bytes=0;this.active=null;this.last=null;this.lastIdr=null;this.shape=null;this.synced=false;
    this.generation=0;this.nextToken=0;this.gapSince=null;this.lastRequest=null;this.time=0;this.head=null;
    this.stats={offered:0,duplicate:0,stale:0,malformed:0,awaitingIdr:0,gaps:0,knownLoss:0,
      expired:0,capacityDrops:0,dependencyDrops:0,idrSuperseded:0,decodeStarted:0,decodeFailed:0,
      decodeApproved:0,lateDecodeRejected:0,idrRecovery:0,recoveryRequestsProposed:0,headObservations:0,headTimeouts:0};
  }
  _time(now){require(Number.isFinite(now)&&now>=this.time,'monotonic local milliseconds');this.time=now;}
  _remove(sequence){const row=this.pending.get(sequence);if(row){this.pending.delete(sequence);this.bytes-=row.bytes.byteLength;}return row;}
  _idr(){let result=null;for(const row of this.pending.values())if(row.packet.keyframe&&(!result||newer(row.packet.sequence,result.packet.sequence)))result=row;return result;}
  _invalidate(){
    this.synced=false;this.generation++;this.gapSince=null;
    for(const [sequence,row]of this.pending)if(!row.packet.keyframe){this._remove(sequence);this.stats.dependencyDrops++;}
  }
  _expire(now){
    let lost=false;
    for(const [sequence,row]of this.pending)if(now-row.arrival>=LIMITS.maxAgeMs){this._remove(sequence);this.stats.expired++;lost=true;}
    if(lost)this._invalidate();
    if(this.head&&now-this.head.observed>=LIMITS.maxAgeMs){this.head=null;this.stats.headTimeouts++;this._invalidate();}
  }
  observeHead(sequence,now){
    this._time(now);require(u32(sequence),'actual future reliable encoded-head sequence');this.stats.headObservations++;
    if(this.last!==null&&!newer(sequence,this.last))return;
    // Preserve the first unfulfilled watermark; newer metadata cannot extend it.
    this.head??={sequence,observed:now};
  }
  loss(sequence,now){
    this._time(now);require(sequence===null||u32(sequence),'known missing sequence or explicitly unknown loss');this.stats.knownLoss++;
    const anchor=this._idr()??(this.active?.packet.keyframe?this.active:null);
    if(sequence!==null&&((this.last!==null&&!newer(sequence,this.last))||(anchor&&newer(anchor.packet.sequence,sequence))))return;
    this._invalidate();
  }
  offer(buffer,now){
    this._time(now);this._expire(now);this.stats.offered++;
    let bytes,packet;
    try{require(buffer instanceof ArrayBuffer&&buffer.byteLength<=LIMITS.frameBytes,'bounded actual GLV1 buffer');
      bytes=new Uint8Array(buffer).slice();packet=parseVideoPacket(bytes.buffer);}
    catch{this.stats.malformed++;this._invalidate();return false;}
    const sequence=packet.sequence;
    if((this.last!==null&&!newer(sequence,this.last))||(this.active&&!newer(sequence,this.active.packet.sequence))){this.stats.stale++;return false;}
    const duplicate=this.pending.get(sequence);
    if(duplicate){if(same(bytes,duplicate.bytes)){this.stats.duplicate++;return false;}this.stats.malformed++;this._invalidate();return false;}
    if(packet.keyframe&&this.lastIdr!==null&&!newer(sequence,this.lastIdr)){this.stats.stale++;return false;}
    const anchor=this.active?.packet.sequence??this._idr()?.packet.sequence??this.last;
    if(packet.keyframe&&anchor!==null&&!newer(sequence,anchor)){this.stats.stale++;return false;}
    if(!packet.keyframe){
      if((!this.synced&&!this.active?.packet.keyframe&&!this._idr())||anchor===null){this.stats.awaitingIdr++;return false;}
      if(!newer(sequence,anchor)||((sequence-anchor)>>>0)>LIMITS.reorderFrames){this.stats.gaps++;this._invalidate();return false;}
    }
    if(bytes.byteLength+this.bytes>LIMITS.pendingBytes||this.pending.size>=LIMITS.pendingFrames){
      this.stats.capacityDrops++;this._invalidate();
      if(!packet.keyframe)return false;
      for(const key of this.pending.keys()){this._remove(key);this.stats.idrSuperseded++;}
    }
    if(packet.keyframe)this.lastIdr=sequence;
    this.pending.set(sequence,{packet,bytes,arrival:now});this.bytes+=bytes.byteLength;return true;
  }
  next(now){
    this._time(now);this._expire(now);if(this.active)return null;
    let row=this._idr();
    if(row){
      for(const [sequence,other]of this.pending)if(sequence!==row.packet.sequence&&!newer(sequence,row.packet.sequence)){
        this._remove(sequence);if(other.packet.keyframe)this.stats.idrSuperseded++;else this.stats.dependencyDrops++;
      }
    }else if(this.synced){
      row=this.pending.get((this.last+1)>>>0);
      if(!row&&this.pending.size){
        this.gapSince??=Math.min(...Array.from(this.pending.values(),value=>value.arrival));
        if(now-this.gapSince>=LIMITS.reorderMs){this.stats.gaps++;this._invalidate();}
      }
    }
    if(!row)return null;
    if(!row.packet.keyframe&&(!this.synced||row.packet.sequence!==((this.last+1)>>>0)||
      row.packet.width!==this.shape.width||row.packet.height!==this.shape.height)){
      this.stats.gaps++;this._invalidate();return null;
    }
    this._remove(row.packet.sequence);this.gapSince=null;
    const token=Object.freeze({id:++this.nextToken,generation:this.generation});
    this.active={...row,token,generation:this.generation};this.stats.decodeStarted++;
    return{token,buffer:row.bytes.buffer,sequence:row.packet.sequence,keyframe:row.packet.keyframe,
      width:row.packet.width,height:row.packet.height,resetDecoder:row.packet.keyframe};
  }
  complete(token,ok,now){
    this._time(now);require(typeof ok==='boolean'&&this.active?.token===token,'one exact outstanding actual decoder completion');
    const row=this.active;this.active=null;
    if(!ok){this.stats.decodeFailed++;this._invalidate();return{paintEligible:false,reason:'decoder-error'};}
    if(row.generation!==this.generation||now-row.arrival>=LIMITS.maxAgeMs){
      this.stats.lateDecodeRejected++;this._invalidate();return{paintEligible:false,reason:'retired-or-expired-decode'};
    }
    if(row.packet.keyframe){if(!this.synced)this.stats.idrRecovery++;this.synced=true;this.shape={width:row.packet.width,height:row.packet.height};}
    require(this.synced&&(row.packet.keyframe||row.packet.sequence===((this.last+1)>>>0)),'contiguous decoded reference chain');
    this.last=row.packet.sequence;
    if(this.head&&(this.last===this.head.sequence||newer(this.last,this.head.sequence)))this.head=null;
    this.stats.decodeApproved++;return{paintEligible:true,reason:'decoded-contiguous-reference-chain'};
  }
  recovery(now){
    this._time(now);this._expire(now);
    if(this.synced||this._idr()||this.active?.packet.keyframe||
      (this.lastRequest!==null&&now-this.lastRequest<LIMITS.recoveryIntervalMs))return null;
    this.lastRequest=now;this.stats.recoveryRequestsProposed++;
    return{type:'resync'}; // Proposed only; caller must separately attest actual write.
  }
  deadline(){
    const ages=Array.from(this.pending.values(),row=>row.arrival+LIMITS.maxAgeMs);
    if(this.gapSince!==null)ages.push(this.gapSince+LIMITS.reorderMs);
    if(this.head)ages.push(this.head.observed+LIMITS.maxAgeMs);
    return ages.length?Math.min(...ages):null;
  }
  snapshot(){return{state:this.synced?'reference-chain-valid':'waiting-for-idr',lastDecodedSequence:this.last,
    pendingFrames:this.pending.size,pendingBytes:this.bytes,activeSequence:this.active?.packet.sequence??null,
    activeBytes:this.active?.bytes.byteLength??0,generation:this.generation,...this.stats};}
}

// GLD1 outer framing stays byte-for-byte unchanged. Complete P access units
// enter the reference gate without being treated as IDRs.
export class PredictiveAssembly{
  constructor({onRetire=()=>{}}={}){require(typeof onRetire==='function','assembly retirement callback');
    this.onRetire=onRetire;this.partial=new Map();this.retired=new Map();this.highest=null;this.time=0;
    this.stats={fragments:0,complete:0,malformed:0,duplicate:0,expired:0,evicted:0,stale:0};}
  _retire(sequence){this.partial.delete(sequence);this.retired.set(sequence,true);while(this.retired.size>LIMITS.reorderFrames)this.retired.delete(this.retired.keys().next().value);this.onRetire(sequence);}
  retire(sequence){require(u32(sequence),'bounded assembly retirement sequence');this._retire(sequence);}
  expire(now){require(Number.isFinite(now)&&now>=this.time,'monotonic assembly milliseconds');this.time=now;
    const losses=[];for(const [sequence,row]of this.partial)if(now-row.arrival>=LIMITS.maxAgeMs){this._retire(sequence);this.stats.expired++;losses.push(sequence);}return losses;}
  push(value,now){
    const losses=this.expire(now);this.stats.fragments++;let failedSequence=null;
    try{
      require(value instanceof Uint8Array&&value.byteLength>=20&&value.byteLength<=1020,'bounded actual GLD1 datagram');
      const view=new DataView(value.buffer,value.byteOffset,value.byteLength),sequence=view.getUint32(4,true),total=view.getUint32(8,true),
        index=view.getUint16(12,true),count=view.getUint16(14,true),stride=view.getUint16(16,true);
      require(view.getUint32(0,true)===0x31444c47&&view.getUint16(18,true)===0&&total>=24&&total<=LIMITS.frameBytes&&
        stride>=256&&stride<=1000&&count===Math.ceil(total/stride)&&index<count&&
        value.byteLength===20+Math.min(stride,total-index*stride),'canonical unchanged GLD1 fragment');
      failedSequence=sequence;
      if(this.retired.has(sequence)||(this.highest!==null&&!newer(sequence,this.highest)&&sequence!==this.highest&&
        ((this.highest-sequence)>>>0)>LIMITS.reorderFrames)){this.stats.stale++;return{buffer:null,losses};}
      if(this.highest===null||newer(sequence,this.highest))this.highest=sequence;
      let row=this.partial.get(sequence);
      if(!row){
        if(this.partial.size>=LIMITS.partialFrames){const oldest=Array.from(this.partial).reduce((a,b)=>a[1].arrival<=b[1].arrival?a:b)[0];this._retire(oldest);this.stats.evicted++;losses.push(oldest);}
        row={total,count,stride,arrival:now,bytes:new Uint8Array(total),seen:new Uint8Array(count),received:0};this.partial.set(sequence,row);
      }
      require(row.total===total&&row.count===count&&row.stride===stride,'consistent fragment metadata');
      const payload=value.subarray(20),offset=index*stride;
      if(row.seen[index]){require(same(payload,row.bytes.subarray(offset,offset+payload.length)),'identical duplicate fragment');this.stats.duplicate++;return{buffer:null,losses};}
      row.bytes.set(payload,offset);row.seen[index]=1;row.received++;
      if(row.received!==count)return{buffer:null,losses};
      this._retire(sequence);const packet=parseVideoPacket(row.bytes.buffer);require(packet.sequence===sequence,'actual GLV1/GLD1 sequence join');
      this.stats.complete++;return{buffer:row.bytes.buffer,losses};
    }catch{if(failedSequence!==null)this._retire(failedSequence);this.stats.malformed++;losses.push(failedSequence);return{buffer:null,losses};}
  }
  snapshot(){return{...this.stats,partialFrames:this.partial.size,partialBytes:Array.from(this.partial.values()).reduce((sum,row)=>sum+row.total,0),retiredFrames:this.retired.size};}
  hasPartial(sequence){return this.partial.has(sequence);}
  partialSequences(){return Array.from(this.partial.keys());}
  retainedBytes(){return Array.from(this.partial.values()).reduce((sum,row)=>sum+row.bytes.byteLength+row.seen.byteLength,0);}
  close(){this.partial.clear();this.retired.clear();this.highest=null;this.time=0;}
}

export class PredictiveReceiver{
  constructor({onAssemblyRetire=()=>{}}={}){this.assembly=new PredictiveAssembly({onRetire:onAssemblyRetire});this.gate=new PredictiveGate();this.acceptedBytes=0;this.closed=false;}
  _live(){require(!this.closed,'closed predictive receiver');}
  pushFragment(value,now){this._live();return this.assembly.push(value,now);}
  expireAssembly(now){this._live();return this.assembly.expire(now);}
  offerAccessUnit(buffer,now){this._live();const admitted=this.gate.offer(buffer,now);if(admitted)this.acceptedBytes+=buffer.byteLength;return admitted;}
  receive(value,now){const result=this.pushFragment(value,now);for(const sequence of result.losses)this.gate.loss(sequence,now);
    return result.buffer?this.offerAccessUnit(result.buffer,now):false;}
  next(now){this._live();for(const sequence of this.assembly.expire(now))this.gate.loss(sequence,now);return this.gate.next(now);}
  complete(token,ok,now){this._live();return this.gate.complete(token,ok,now);}
  observeHead(sequence,now){this._live();this.gate.observeHead(sequence,now);}
  loss(sequence,now){this._live();this.gate.loss(sequence,now);}
  retire(sequence){this._live();this.assembly.retire(sequence);}
  hasPartial(sequence){this._live();return this.assembly.hasPartial(sequence);}
  partialSequences(){this._live();return this.assembly.partialSequences();}
  retainedBytes(){return this.assembly.retainedBytes()+this.gate.bytes+(this.gate.active?.bytes.byteLength??0);}
  recovery(now){this._live();for(const sequence of this.assembly.expire(now))this.gate.loss(sequence,now);return this.gate.recovery(now);}
  snapshot(){return{assembly:this.assembly.snapshot(),reference:this.gate.snapshot()};}
  close(){this._live();this.assembly.close();this.gate.pending.clear();this.gate.bytes=0;this.gate.active=null;
    this.gate.last=null;this.gate.lastIdr=null;this.gate.shape=null;this.gate.synced=false;this.gate.generation=0;
    this.gate.nextToken=0;this.gate.gapSince=null;this.gate.lastRequest=null;this.gate.time=0;this.gate.head=null;
    this.acceptedBytes=0;this.closed=true;}
}
