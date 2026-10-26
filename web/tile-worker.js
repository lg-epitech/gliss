import {TileReceiver,newer,MAX_COMPLETE_BYTES,MAX_TILES,MAX_AGE_MS} from './tile-decoder.js';

const require=(ok,message)=>{if(!ok)throw new Error(message);};
// Two bounded task waiters: one receiver and one sequential decoder. A
// MessageChannel yields to actual tasks without nested-timer minimum delays.
export class TaskYield{
  constructor(channelFactory=()=>new MessageChannel()){
    this.channel=channelFactory();this.pending=[];this.closed=false;
    this.channel.port1.onmessage=()=>{if(!this.closed)this.pending.shift()?.();};this.channel.port1.start?.();
  }
  wait(){if(this.closed)return Promise.resolve();require(this.pending.length<2,'task-yield waiter bound');return new Promise(resolve=>{this.pending.push(resolve);this.channel.port2.postMessage(null);});}
  close(){if(this.closed)return;this.closed=true;this.channel.port1.close();this.channel.port2.close();for(const resolve of this.pending.splice(0))resolve();}
}

// Exported to exercise the real bounded scheduler without a browser mock codec.
export class TileWorkerCore{
  constructor({config,post,now=()=>performance.now(),inflate}){
    this.now=now;this.post=post;this.receiver=new TileReceiver({...config,now,...(inflate?{inflate}:{})});this.queue=new Map();this.bytes=0;this.cursor=0;
    this.inflight=null;this.batchId=0;this.decoding=false;this.closed=false;this.reader=null;this.epoch=null;this.taskYield=new TaskYield();
    this.stats={patchesQueued:0,patchesReplaced:0,patchesEvicted:0,patchesExpired:0,patchBytesPeak:0,patchCountPeak:0,transfers:0,transferTiles:0,acceptedTransfers:0,transferBytesPeak:0,receiveYields:0,decodeYields:0};
  }
  _remove(tile){const patch=this.queue.get(tile);if(patch){this.queue.delete(tile);this.bytes-=patch.pixels.byteLength;}return patch;}
  _queue(patch){
    if(this.epoch!==patch.epoch){if(this.epoch!==null&&!newer(patch.epoch,this.epoch))return;this.epoch=patch.epoch;for(const tile of this.queue.keys())this._remove(tile);}
    const old=this.queue.get(patch.tile);if(old){if(!newer(patch.sequence,old.sequence))return;this._remove(patch.tile);this.stats.patchesReplaced++;}
    while(this.bytes+patch.pixels.byteLength>MAX_COMPLETE_BYTES||this.queue.size>=MAX_TILES){let victim=null,oldest=Infinity;for(const[tile,value]of this.queue)if(value.arrival<oldest){victim=tile;oldest=value.arrival;}require(victim!==null,'decoded patch bound');this._remove(victim);this.stats.patchesEvicted++;}
    this.queue.set(patch.tile,patch);this.bytes+=patch.pixels.byteLength;this.stats.patchesQueued++;this.stats.patchBytesPeak=Math.max(this.stats.patchBytesPeak,this.bytes);this.stats.patchCountPeak=Math.max(this.stats.patchCountPeak,this.queue.size);this._transfer();
  }
  _transfer(){
    if(this.closed||this.inflight||!this.queue.size)return;
    for(const[tile,patch]of this.queue)if(this.now()-patch.arrival>=MAX_AGE_MS){this._remove(tile);this.stats.patchesExpired++;}
    const patches=[];let bytes=0;
    for(let visited=0;visited<MAX_TILES&&patches.length<64;visited++){
      const tile=this.cursor;this.cursor=(this.cursor+1)%MAX_TILES;const patch=this.queue.get(tile);if(!patch)continue;
      if(bytes+patch.pixels.byteLength>262144)break;this._remove(tile);patches.push(patch);bytes+=patch.pixels.byteLength;
    }
    if(!patches.length)return;const id=this.batchId=(this.batchId+1)>>>0;this.inflight={id,bytes,tiles:patches.length};
    this.stats.transfers++;this.stats.transferTiles+=patches.length;this.stats.transferBytesPeak=Math.max(this.stats.transferBytesPeak,bytes);
    this.post({type:'patches',batchId:id,patches},patches.map(patch=>patch.pixels.buffer));
  }
  accepted(batchId){require(this.inflight&&this.inflight.id===batchId,'unexpected transfer acceptance');this.inflight=null;this.stats.acceptedTransfers++;this._transfer();}
  receive(packet,arrival=this.now()){const ready=this.receiver.receive(packet,arrival);if(ready)this._kick();return ready;}
  _kick(){if(this.closed||this.decoding)return;this.decoding=true;void this._decode().catch(error=>this.post({type:'fatal',message:error.message})).finally(()=>{this.decoding=false;if(!this.closed&&this.receiver.complete.size)this._kick();});}
  async _decode(){let count=0;while(!this.closed&&this.receiver.complete.size){const patch=await this.receiver.decodeNext();if(this.closed)return;if(patch)this._queue(patch);if(++count%64===0){this.stats.decodeYields++;await this.taskYield.wait();}}}
  async read(readable){
    require(!this.reader&&readable&&typeof readable.getReader==='function','one transferred datagram reader required');this.reader=readable.getReader();let count=0;
    try{while(!this.closed){const{value,done}=await this.reader.read();if(done)break;this.receive(value,this.now());if(++count%64===0){this.stats.receiveYields++;await this.taskYield.wait();}}}
    finally{this.reader.releaseLock();this.reader=null;if(!this.closed)this.post({type:'media-ended'});}
  }
  snapshot(){return{...this.stats,receiver:this.receiver.snapshot(),patchCount:this.queue.size,patchBytes:this.bytes,inflight:this.inflight,decodedEpoch:this.receiver.epoch,decodedSequences:this.receiver.sequences.slice()};}
  async close(){if(this.closed)return;this.closed=true;this.taskYield.close();this.receiver.close();for(const tile of this.queue.keys())this._remove(tile);if(this.reader)await this.reader.cancel().catch(()=>{});this.inflight=null;}
}

if(typeof self!=='undefined'&&typeof self.postMessage==='function'&&typeof document==='undefined'){
  let core=null,session=null;
  self.onmessage=event=>{
    const message=event.data;
    void(async()=>{
      if(message?.type==='start'){
        require(!core,'worker may start once');session=message.session;require(Number.isFinite(message.mainTimeOrigin),'browser-local time origin required');
        const now=()=>performance.timeOrigin+performance.now()-message.mainTimeOrigin;
        core=new TileWorkerCore({config:message.config,now,post:(value,transfer=[])=>self.postMessage({...value,session},transfer)});
        self.postMessage({type:'started',session});void core.read(message.readable).catch(error=>self.postMessage({type:'fatal',session,message:error.message}));
      }else{
        require(core&&message?.session===session,'worker session mismatch');
        if(message.type==='accepted')core.accepted(message.batchId);
        else if(message.type==='stats')self.postMessage({type:'stats',session,requestId:message.requestId,value:core.snapshot()});
        else if(message.type==='stop'){await core.close();self.postMessage({type:'stopped',session});self.close();}
        else throw new Error('unknown tile worker control');
      }
    })().catch(error=>self.postMessage({type:'fatal',session,message:error.message}));
  };
}
