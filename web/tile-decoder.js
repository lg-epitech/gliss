// CPU live GLT2: independently bounded receive, inflate and patch queues.
// GLT2 transform/wire semantics stay fixed; the default inflater is bounded CPU JS.
import {boundedFflate} from './tile-inflate.js';
export const TILE=32,HEADER=32,MAX_TILES=8160,MAX_PARTIALS=128,MAX_PARTIAL_BYTES=524288,MAX_COMPLETE_BYTES=4194304;
export const PARTIAL_TTL_MS=134,MAX_AGE_MS=250;
export const newer=(a,b)=>a!==b&&((a-b)>>>0)<0x80000000;
const require=(ok,message)=>{if(!ok)throw new Error(message);};
export const tileCount=(w,h)=>Math.ceil(w/TILE)*Math.ceil(h/TILE);
export const tileRect=(w,h,tile)=>{const x=tile%Math.ceil(w/TILE)*TILE,y=Math.floor(tile/Math.ceil(w/TILE))*TILE;return{x,y,width:Math.min(TILE,w-x),height:Math.min(TILE,h-y)};};
export function inversePredicted(predicted, width, height, policy) {
  require(predicted instanceof Uint8Array && Number.isInteger(width) && width >= 1 && width <= TILE && Number.isInteger(height) && height >= 1 && height <= TILE && Number.isInteger(policy) && policy >= 0 && policy <= 2 && predicted.length === width * height * 4, 'invalid predicted tile');
  const area = width * height, planes = new Uint8Array(predicted.length);
  for (let channel = 0; channel < 4; channel++) {
    for (let row = 0; row < height; row++) {
      let left = 0;
      for (let x = 0; x < width; x++) {
        const at = channel * area + row * width + x;
        left = (left + predicted[at]) & 255;
        planes[at] = left;
      }
    }
  }
  const out = new Uint8Array(predicted.length), clamp = value => Math.max(0, Math.min(255, value));
  for (let i = 0; i < area; i++) {
    if (policy === 0) {
      const g = planes[i];
      out[i * 4] = (g + planes[area + i]) & 255;
      out[i * 4 + 1] = g;
      out[i * 4 + 2] = (g + planes[2 * area + i]) & 255;
    } else {
      const bound = policy === 1 ? 16 : 8, scale = policy === 1 ? 16 : 32;
      const coQ = planes[area + i] - 128, cgQ = planes[2 * area + i] - 128;
      require(coQ >= -bound && coQ <= bound && cgQ >= -bound && cgQ <= bound, 'invalid quantized chroma coefficient');
      const co = coQ * scale, cg = cgQ * scale;
      const t = planes[i] - Math.floor(cg / 2);
      const g = cg + t, b = t - Math.floor(co / 2), r = b + co;
      out[i * 4] = clamp(r);
      out[i * 4 + 1] = clamp(g);
      out[i * 4 + 2] = clamp(b);
    }
    out[i * 4 + 3] = planes[3 * area + i];
  }
  return out;
}


export async function browserInflate(body, limit) {
  require(body instanceof Uint8Array && body.length > 0 && body.length <= 4096 && Number.isInteger(limit) && limit > 0 && limit <= 4096, 'invalid inflate bounds');
  const stream = new Blob([body]).stream().pipeThrough(new DecompressionStream('deflate'));
  const reader = stream.getReader(), chunks = [];
  let total = 0;
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      total += value.length;
      require(total <= limit, 'oversized zlib tile');
      chunks.push(value);
    }
  } catch (error) {
    await reader.cancel().catch(() => {});
    throw error;
  } finally {
    reader.releaseLock();
  }
  const out = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) { out.set(chunk, offset); offset += chunk.length; }
  return out;
}

export function parseTile(packet,stride=1168,requestedPolicy=2){
  require(Number.isInteger(stride)&&stride>=1024&&stride<=1168,'invalid trusted payload stride');
  require(Number.isInteger(requestedPolicy)&&requestedPolicy>=0&&requestedPolicy<=2,'invalid negotiated policy');
  require(packet instanceof Uint8Array&&packet.length>HEADER&&packet.length<=HEADER+stride,'invalid datagram size');
  const view=new DataView(packet.buffer,packet.byteOffset,packet.byteLength),u16=n=>view.getUint16(n,true),u32=n=>view.getUint32(n,true);
  require(u32(0)===0x32544c47,'invalid tile magic');
  const h={epoch:u32(4),sequence:u32(8),width:u16(12),height:u16(14),tile:u16(16),kind:packet[19],policy:packet[30],total:u16(20),index:packet[22],count:packet[23],offset:u16(24),raw:u16(28)};
  require(h.width>0&&h.width<=3840&&h.height>0&&h.height<=2160&&tileCount(h.width,h.height)<=MAX_TILES,'invalid geometry');
  require(packet[18]===TILE&&h.kind<=2&&h.policy<=2&&packet[31]===0&&(h.kind===2||h.policy===0)&&(h.policy===0||h.policy===requestedPolicy),'unsupported policy/flags');
  require(h.tile<tileCount(h.width,h.height),'tile outside geometry');const r=tileRect(h.width,h.height,h.tile);
  require(h.raw===r.width*r.height*4,'invalid raw tile length');require(h.total>0&&h.total<=h.raw&&(h.kind!==0||h.total===h.raw),'invalid encoded length');
  require(h.count===Math.ceil(h.total/stride)&&h.count>=1&&h.count<=4&&h.index<h.count,'invalid fragment count/index');
  require(h.offset===h.index*stride&&u16(26)===Math.min(stride,h.total-h.offset)&&packet.length===HEADER+u16(26),'noncanonical fragment');return h;
}

export class TileReceiver{
  constructor({payloadStride=1168,policy=0,now=()=>performance.now(),inflate=boundedFflate}={}){
    require(Number.isInteger(payloadStride)&&payloadStride>=1024&&payloadStride<=1168,'invalid trusted stride');require(Number.isInteger(policy)&&policy>=0&&policy<=2,'invalid policy');
    this.stride=payloadStride;this.policy=policy;this.now=now;this.inflate=inflate;this.epoch=null;this.width=0;this.height=0;this.sequences=[];
    this.partial=new Map();this.partialBytes=0;this.complete=new Map();this.completeBytes=0;this.geometry=new Map();this.cursor=0;this.active=null;this.closed=false;this.generation=0;
    this.stats={datagrams:0,receivedBytes:0,malformed:0,duplicates:0,stale:0,partialExpired:0,partialEvicted:0,completeExpired:0,completeEvicted:0,completeReplaced:0,completed:0,inflateAttempted:0,inflateCompleted:0,inflateFailed:0,lateRejected:0,decodedTiles:0,decodedBytes:0,epochCommits:0,partialPeak:0,partialBytesPeak:0,completePeak:0,completeBytesPeak:0,activePeak:0};
  }
  _ref(h,delta){let value=this.geometry.get(h.epoch);if(!value){require(delta===1,'missing staged geometry');value={width:h.width,height:h.height,refs:0};this.geometry.set(h.epoch,value);}value.refs+=delta;if(value.refs===0)this.geometry.delete(h.epoch);}
  _partialRemove(key){const item=this.partial.get(key);if(item){this.partial.delete(key);this.partialBytes-=item.body.length;this._ref(item.h,-1);}return item;}
  _completeRemove(tile){const item=this.complete.get(tile);if(item){this.complete.delete(tile);this.completeBytes-=item.body.length;this._ref(item.h,-1);}return item;}
  _peaks(){const s=this.stats;s.partialPeak=Math.max(s.partialPeak,this.partial.size);s.partialBytesPeak=Math.max(s.partialBytesPeak,this.partialBytes);s.completePeak=Math.max(s.completePeak,this.complete.size);s.completeBytesPeak=Math.max(s.completeBytesPeak,this.completeBytes);s.activePeak=Math.max(s.activePeak,Number(Boolean(this.active)));}
  expire(){const now=this.now();for(const[key,item]of this.partial)if(now-item.arrival>=PARTIAL_TTL_MS){this._partialRemove(key);this.stats.partialExpired++;}for(const[tile,item]of this.complete)if(now-item.arrival>=MAX_AGE_MS){this._completeRemove(tile);this.stats.completeExpired++;}}
  _old(h){if(this.epoch===null)return false;if(h.epoch!==this.epoch)return!newer(h.epoch,this.epoch);return this.sequences[h.tile]!==null&&!newer(h.sequence,this.sequences[h.tile]);}
  _newerItem(a,b){return a.epoch===b.epoch?newer(a.sequence,b.sequence):newer(a.epoch,b.epoch);}
  receive(packet,arrival=this.now()){
    if(this.closed)return false;this.stats.datagrams++;this.stats.receivedBytes+=packet?.byteLength??0;this.expire();
    try{
      require(Number.isFinite(arrival)&&arrival<=this.now()&&this.now()-arrival<MAX_AGE_MS,'invalid/expired local arrival');const h=parseTile(packet,this.stride,this.policy);
      if(h.epoch===this.epoch)require(h.width===this.width&&h.height===this.height,'committed epoch geometry conflict');
      if(this._old(h)){this.stats.stale++;return false;}
      const shape=this.geometry.get(h.epoch);if(shape)require(shape.width===h.width&&shape.height===h.height,'staged epoch geometry conflict');
      const queued=this.complete.get(h.tile);if(queued&&!this._newerItem(h,queued.h)){this.stats.stale++;return false;}
      const key=`${h.epoch}:${h.tile}`;let item=this.partial.get(key);
      if(item){if(item.h.sequence!==h.sequence){if(!newer(h.sequence,item.h.sequence)){this.stats.stale++;return false;}this._partialRemove(key);item=null;}
        else require(['kind','policy','total','raw','count'].every(field=>item.h[field]===h[field]),'conflicting fragment metadata');}
      if(!item){
        while(this.partial.size>=MAX_PARTIALS||this.partialBytes+h.total>MAX_PARTIAL_BYTES){let victim=null,oldest=Infinity;for(const[k,p]of this.partial)if(p.arrival<oldest){victim=k;oldest=p.arrival;}require(victim!==null,'partial allocation bound');this._partialRemove(victim);this.stats.partialEvicted++;}
        item={h,body:new Uint8Array(h.total),seen:0,arrival};this.partial.set(key,item);this.partialBytes+=h.total;this._ref(h,1);
      }
      const bit=1<<h.index,payload=packet.subarray(HEADER);if(item.seen&bit){require(payload.every((byte,index)=>byte===item.body[h.offset+index]),'conflicting duplicate');this.stats.duplicates++;return false;}
      item.body.set(payload,h.offset);item.seen|=bit;this._peaks();if(item.seen!==(1<<h.count)-1)return false;
      this._partialRemove(key);const previous=this.complete.get(h.tile);if(previous){this._completeRemove(h.tile);this.stats.completeReplaced++;}
      while(this.completeBytes+item.body.length>MAX_COMPLETE_BYTES||this.complete.size>=MAX_TILES){let victim=null,oldest=Infinity;for(const[tile,p]of this.complete)if(p.arrival<oldest){victim=tile;oldest=p.arrival;}require(victim!==null,'complete allocation bound');this._completeRemove(victim);this.stats.completeEvicted++;}
      this.complete.set(h.tile,item);this.completeBytes+=item.body.length;this._ref(h,1);this.stats.completed++;this._peaks();return true;
    }catch{this.stats.malformed++;return false;}
  }
  async decodeNext(){
    require(!this.active,'only one inflate may be active');if(this.closed)return null;this.expire();let item=null;
    for(let visited=0;visited<MAX_TILES;visited++){const tile=this.cursor;this.cursor=(this.cursor+1)%MAX_TILES;if(this.complete.has(tile)){item=this._completeRemove(tile);break;}}
    if(!item)return null;this.active=item;this._ref(item.h,1);this._peaks();const token=this.generation,h=item.h,start=this.now();
    try{
      let bytes;if(h.kind===0)bytes=item.body;else{this.stats.inflateAttempted++;bytes=await this.inflate(item.body,h.raw);this.stats.inflateCompleted++;}
      require(bytes instanceof Uint8Array&&bytes.length===h.raw,'inflate exact output length');if(h.kind===2)bytes=inversePredicted(bytes,tileRect(h.width,h.height,h.tile).width,tileRect(h.width,h.height,h.tile).height,h.policy);
      const queued=this.complete.get(h.tile);
      if(this.closed||token!==this.generation||this.now()-item.arrival>=MAX_AGE_MS||this._old(h)||queued&&this._newerItem(queued.h,h)){this.stats.lateRejected++;return null;}
      // Geometry/epoch becomes committed only after a complete, validated body.
      if(this.epoch!==h.epoch){this.epoch=h.epoch;this.width=h.width;this.height=h.height;this.sequences=Array(tileCount(h.width,h.height)).fill(null);this.stats.epochCommits++;
        for(const[key,p]of this.partial)if(p.h.epoch!==h.epoch&&!newer(p.h.epoch,h.epoch))this._partialRemove(key);
        for(const[tile,p]of this.complete)if(p.h.epoch!==h.epoch&&!newer(p.h.epoch,h.epoch))this._completeRemove(tile);
      }
      this.sequences[h.tile]=h.sequence;this.stats.decodedTiles++;this.stats.decodedBytes+=bytes.length;
      return {...h,...tileRect(h.width,h.height,h.tile),frameWidth:h.width,frameHeight:h.height,arrival:item.arrival,decoded:this.now(),decodeMs:this.now()-start,pixels:new Uint8ClampedArray(bytes)};
    }catch{this.stats.inflateFailed++;return null;}finally{this._ref(h,-1);this.active=null;}
  }
  snapshot(){this.expire();let oldest=null;for(const p of[...this.partial.values(),...this.complete.values()])oldest=oldest===null?p.arrival:Math.min(oldest,p.arrival);return{...this.stats,epoch:this.epoch,width:this.width,height:this.height,partialCount:this.partial.size,partialBytes:this.partialBytes,completeCount:this.complete.size,completeBytes:this.completeBytes,activeInflates:Number(Boolean(this.active)),oldestLocalAgeMs:oldest===null?null:this.now()-oldest};}
  close(){this.closed=true;this.generation++;for(const key of this.partial.keys())this._partialRemove(key);for(const tile of this.complete.keys())this._completeRemove(tile);this.sequences=[];}
}

