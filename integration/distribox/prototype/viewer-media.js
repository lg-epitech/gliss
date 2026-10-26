// Own only transport/lifecycle policy; wire and video decoding stay in web/*.
export const MAX_MEDIA_BYTES=12*1024*1024, MAX_PENDING_MEDIA=2;
const sameKeys=(value,expected)=>value&&typeof value==='object'&&!Array.isArray(value)&&
  Object.keys(value).length===Object.keys(expected).length&&Object.entries(expected).every(([key,item])=>value[key]===item);

export function codecFromHello(value){
  if(value?.type!=='hello'||value.source!=='rfb'||value.fps_limit!==30)throw new Error('Invalid native desktop hello');
  if(value.codec==='deflate'&&value.protocol==='GLS1'&&value.hybrid_config===undefined)return 'deflate';
  const expected={base_mode:'global-cached',video_profile:'camera-scene-off',video_usage:'camera-video-real-time',
    scene_change_detection:false,mask_grid:32,base_tile:64};
  if(value.codec==='hybrid-continuity'&&value.protocol==='GLH4'&&sameKeys(value.hybrid_config,expected))return 'hybrid-continuity';
  throw new Error('Native codec/profile is outside the trusted gateway presets');
}

// This is only a transport admission gate. The existing decoder must still
// validate every nested/reference declaration before a key frame can paint.
export function independentKeyPacket(packet,codec){
  if(!['deflate','hybrid-continuity'].includes(codec)||!(packet instanceof ArrayBuffer)||packet.byteLength<(codec==='hybrid-continuity'?64:24))return false;
  const bytes=new Uint8Array(packet),magic=String.fromCharCode(...bytes.subarray(0,4));
  if(magic!==(codec==='hybrid-continuity'?'GLH4':'GLS1'))return false;
  const flags=new DataView(packet).getUint16(18,true);
  return Boolean(flags&1)&&!(flags&~(codec==='hybrid-continuity'?7:1));
}

export async function loadDecoder(codec){
  if(codec==='deflate'){const module=await import('./decoder.js');return new module.Decoder();}
  if(codec==='hybrid-continuity'){const module=await import('./hybrid-continuity-decoder.js');return new module.HybridContinuityDecoder();}
  throw new Error('Unsupported gateway decoder');
}

const dispose=decoder=>{if(typeof decoder?.close==='function')decoder.close();else decoder?.reset?.();};

export class ViewerMedia{
  constructor({open,send,paint,boundary=()=>new Promise(resolve=>requestAnimationFrame(resolve)),
    loader=loadDecoder,onError=()=>{},metrics,loadTimeoutMs=5000}){
    if(!Number.isFinite(loadTimeoutMs)||loadTimeoutMs<=0)throw new Error('Invalid decoder load deadline');
    Object.assign(this,{open,send,paint,boundary,loader,onError,metrics,loadTimeoutMs});
    this.epoch=0;this.closed=false;this.authorized=false;this.codec=null;this.decoder=null;
    this.queue=[];this.queueBytes=0;this.activeBytes=0;this.busy=false;this.recovering=false;
  }
  capabilities(value){
    if(this.authorized||!Array.isArray(value)||!value.includes('display')||new Set(value).size!==value.length||
      value.some(item=>!['display','input'].includes(item)))throw new Error('Invalid scoped desktop capabilities');
    this.authorized=true;
  }
  hello(value){
    if(!this.authorized||this.codec!==null||this.closed)throw new Error('Desktop hello requires one authorized claim');
    this.codec=codecFromHello(value);this.metrics.codec=this.codec;this.metrics.protocol=value.protocol;
  }
  invalidate(){
    this.epoch++;dispose(this.decoder);this.decoder=null;this.queue=[];this.queueBytes=0;
  }
  reset(){if(this.closed)return;this.invalidate();this.recovering=false;this.metrics.resets++;}
  requestRecovery(){
    if(this.closed||!this.codec||this.recovering)return;
    this.invalidate();this.recovering=true;this.metrics.resyncs++;this.send({type:'resync'});
  }
  close(){if(this.closed)return;this.closed=true;this.invalidate();}
  current(epoch){return !this.closed&&this.epoch===epoch&&this.open();}
  enqueue(packet){
    if(this.closed)return;
    if(!this.authorized||!this.codec||!(packet instanceof ArrayBuffer)||packet.byteLength<24||packet.byteLength>MAX_MEDIA_BYTES)
      throw new Error('Unexpected or oversized native media');
    // Native resync emits an independent key packet, not a reset text event.
    const recoveryKey=this.recovering;
    if(recoveryKey&&!independentKeyPacket(packet,this.codec))return;
    if(this.queue.length+Number(this.busy)>=MAX_PENDING_MEDIA||this.queueBytes+this.activeBytes+packet.byteLength>MAX_PENDING_MEDIA*MAX_MEDIA_BYTES)
      throw new Error('Gateway viewer media queue exceeded its bound');
    if(recoveryKey)this.recovering=false;
    this.queue.push({packet,epoch:this.epoch,recoveryKey});this.queueBytes+=packet.byteLength;
    this.metrics.max_pending_media=Math.max(this.metrics.max_pending_media,this.queue.length+Number(this.busy));
    void this.drain();
  }
  async acquire(epoch){
    if(this.decoder)return this.decoder;
    let expired=false,timer;
    const candidate=Promise.resolve().then(()=>this.loader(this.codec));
    candidate.then(decoder=>{if(expired||!this.current(epoch))dispose(decoder);},()=>{});
    try{
      const decoder=await Promise.race([candidate,new Promise((_,reject)=>{timer=setTimeout(()=>{expired=true;reject(new Error('Gateway decoder loading timed out'));},this.loadTimeoutMs);})]);
      if(!this.current(epoch))return null;
      if(!decoder||typeof decoder.decode!=='function')throw new Error('Invalid gateway decoder');
      this.decoder=decoder;return decoder;
    }finally{clearTimeout(timer);}
  }
  async drain(){
    if(this.busy)return;
    this.busy=true;
    try{
      while(this.queue.length){
        const {packet,epoch,recoveryKey}=this.queue.shift();this.queueBytes-=packet.byteLength;this.activeBytes=packet.byteLength;
        try{
          if(!this.current(epoch))continue;
          const decoder=await this.acquire(epoch);if(!decoder||!this.current(epoch))continue;
          const frame=await decoder.decode(packet);if(!this.current(epoch))continue;
          if(recoveryKey&&!frame.keyframe)throw new Error('Recovery decoder did not return an independent key frame');
          await this.boundary();if(!this.current(epoch))continue;
          this.paint(frame);
          if(recoveryKey){this.metrics.recoveries++;this.metrics.recovery_key_sequences.push(frame.sequence);if(this.metrics.recovery_key_sequences.length>1024)this.metrics.recovery_key_sequences.shift();}
          this.metrics.frames++;this.metrics.bytes+=frame.bytes;
          this.metrics.sequences.push(frame.sequence);if(this.metrics.sequences.length>1024)this.metrics.sequences.shift();
          if(frame.hybridContinuity){
            this.metrics.hybrid_frames++;this.metrics.mask_grid=frame.maskGrid;
            if(frame.videoBytes){this.metrics.hybrid_video_frames++;if(!frame.videoKeyframe)this.metrics.hybrid_video_delta_frames++;}
          }
          // ACK is tied to the same live epoch after actual synchronous paint.
          this.send({type:'ack',sequence:frame.sequence});
        }catch(error){
          if(this.current(epoch)){this.metrics.errors++;this.onError(error);this.requestRecovery();}
        }finally{this.activeBytes=0;}
      }
    }finally{this.busy=false;}
  }
}
