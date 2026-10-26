import {DatagramSocket,controlMessages} from './datagram-transport.js';
import {TilePainter} from './tile-painter.js';

const require=(ok,message)=>{if(!ok)throw new Error(message);};
const policies={'glt2-lossless':0,'glt2-gentle':1,'glt2-strong':2};
function accessor(object,name){for(let prototype=Object.getPrototypeOf(object);prototype;prototype=Object.getPrototypeOf(prototype)){const d=Object.getOwnPropertyDescriptor(prototype,name);if(d)return typeof d.get==='function'&&typeof d.set==='function';}return false;}

export function configureTileDatagrams(datagrams){
  require(datagrams&&accessor(datagrams,'incomingMaxAge'),'actual incomingMaxAge accessor required');
  const property=accessor(datagrams,'incomingMaxBufferedDatagrams')?'incomingMaxBufferedDatagrams':accessor(datagrams,'incomingHighWaterMark')?'incomingHighWaterMark':null;
  require(property,'actual incoming queue-count accessor required');const before={incomingMaxAge:datagrams.incomingMaxAge,[property]:datagrams[property]};
  datagrams.incomingMaxAge=250;datagrams[property]=4096;require(datagrams.incomingMaxAge===250&&datagrams[property]===4096,'browser did not retain incoming queue policy');
  return {actual_prototype_accessors:true,queue_count_property:property,before,requested_incoming_max_age_ms:250,requested_queue_count:4096,incoming_max_age_ms:datagrams.incomingMaxAge,incoming_queue_count:datagrams[property],
    scope:'Actual session accessor assignment/readback; not proof of effective eviction, kernel buffering or network delay.'};
}
export function validateTileHello(descriptor,message){
  const codec=descriptor.datagram_codec??descriptor.codec;
  require(Object.hasOwn(policies,codec)&&(!descriptor.codec||descriptor.codec===codec)&&(!descriptor.datagram_codec||descriptor.datagram_codec===codec),'invalid descriptor codec');
  require(descriptor.protocol==='GLT2-WT1-negotiated-stride'&&descriptor.datagram_protocol==='GLT2'&&descriptor.policy===policies[codec]&&descriptor.congestion_control==='bbr','invalid trusted tile descriptor');
  require(Array.isArray(descriptor.payload_stride_range)&&descriptor.payload_stride_range.length===2&&descriptor.payload_stride_range[0]===1024&&descriptor.payload_stride_range[1]===1168&&descriptor.max_datagram_bytes===1200,'invalid descriptor payload range');
  require(message.transport==='webtransport-datagram'&&message.codec===codec&&message.datagram_codec===codec&&message.protocol===descriptor.protocol&&message.datagram_protocol==='GLT2'&&message.policy===descriptor.policy&&message.tile_side===32&&message.independent_tiles===true&&message.media_retransmission===false&&message.congestion_control==='bbr','invalid authenticated tile hello');
  require(Number.isInteger(message.payload_stride)&&message.payload_stride>=1024&&message.payload_stride<=1168&&message.max_datagram_bytes===message.payload_stride+32,'invalid negotiated payload stride');
  require(Number.isInteger(message.initial_epoch)&&message.initial_epoch>=0&&message.initial_epoch<=0xffffffff&&Number.isInteger(message.width)&&message.width>0&&message.width<=3840&&Number.isInteger(message.height)&&message.height>0&&message.height<=2160,'invalid hello geometry/epoch');
  return {payloadStride:message.payload_stride,policy:message.policy};
}

export class TileMediaSession{
  constructor(canvas,{onPaint=()=>{},onError=()=>{},workerFactory=()=>new Worker(new URL('./tile-worker.js',import.meta.url),{type:'module',name:'gliss-tiles-cpu'})}={}){
    this.session=crypto.randomUUID();this.workerFactory=workerFactory;this.worker=null;this.closed=false;this.requests=new Map();this.requestId=0;this.receiverStats=null;this.cleanup={stopRequested:false,stoppedAck:false,terminated:false};
    this.painter=new TilePainter(canvas,{onPaint,onError});this.onError=onError;
  }
  start(readable,config){
    require(!this.worker&&!this.closed,'tile worker may start once');const worker=this.worker=this.workerFactory();
    worker.onerror=event=>this.onError(new Error(event.message??'CPU tile worker failed'));
    worker.onmessage=event=>{
      const message=event.data;if(message?.session!==this.session)return;
      try{
        if(message.type==='patches'){
          if(this.closed)return;this.painter.acceptBatch(message.patches);
          // Acceptance into the bounded main queue, never a paint completion.
          worker.postMessage({type:'accepted',session:this.session,batchId:message.batchId});
        }else if(message.type==='stats'){
          this.receiverStats=message.value;const request=this.requests.get(message.requestId);if(request){clearTimeout(request.timer);this.requests.delete(message.requestId);request.resolve(message.value);}
        }else if(message.type==='stopped'){this.cleanup.stoppedAck=true;this.resolveStopped?.();}
        else if(message.type==='fatal')throw new Error(message.message);
        else if(message.type==='media-ended'&&!this.closed)throw new Error('tile media ended');
      }catch(error){this.onError(error);}
    };
    try{worker.postMessage({type:'start',session:this.session,config,mainTimeOrigin:performance.timeOrigin,readable},[readable]);}
    catch(error){worker.terminate();this.cleanup.terminated=true;throw error;}
  }
  collectStats(){
    require(this.worker&&!this.closed,'tile worker unavailable');require(this.requests.size<2,'tile telemetry request bound');
    const requestId=this.requestId=(this.requestId+1)>>>0;return new Promise((resolve,reject)=>{
      const timer=setTimeout(()=>{this.requests.delete(requestId);reject(new Error('tile worker telemetry timeout'));},1000);this.requests.set(requestId,{resolve,reject,timer});this.worker.postMessage({type:'stats',session:this.session,requestId});
    });
  }
  snapshotRegion(rect){return this.painter.snapshotRegion(rect);}
  snapshot(){return {paint:this.painter.snapshot(),worker:this.receiverStats,cleanup:{...this.cleanup},semantics:'Painted tile identities change only after actual Canvas putImageData; worker decoded state is separate. Local ages normalize dedicated-worker performance origin to the main browser origin.'};}
  close(){
    if(this.closed)return this.closePromise;this.closed=true;this.painter.close();for(const request of this.requests.values()){clearTimeout(request.timer);request.reject(new Error('tile session closed'));}this.requests.clear();
    if(!this.worker){this.cleanup.terminated=true;return this.closePromise=Promise.resolve();}
    this.cleanup.stopRequested=true;this.closePromise=new Promise(resolve=>{let timer;const done=()=>{clearTimeout(timer);this.worker.terminate();this.cleanup.terminated=true;resolve();};this.resolveStopped=done;timer=setTimeout(done,500);
      try{this.worker.postMessage({type:'stop',session:this.session});}catch{done();}
    });return this.closePromise;
  }
}

export class TileDatagramSocket extends DatagramSocket{
  constructor({descriptor,canvas,onPaint}={}){
    require(canvas&&typeof canvas.getContext==='function'&&canvas.getContext('2d',{alpha:false}),'CPU canvas required before transport construction');
    super({descriptor});clearInterval(this.expiryTimer);this.tileMedia=new TileMediaSession(canvas,{onPaint,onError:error=>this._fail(error)});
    this.transportStats={mediaBytes:0,reliableBytes:0,browserQueuePolicy:null,codec:descriptor?.datagram_codec??descriptor?.codec};this.telemetryTimer=null;
  }
  _configureDatagrams(){this.transportStats.browserQueuePolicy=configureTileDatagrams(this.transport.datagrams);}
  async _readMedia(){} // The transferred readable belongs only to the CPU worker.
  _deliverLatest(){}
  async _readControl(readable){
    for await(const message of controlMessages(readable)){
      if(this.closedOnce)return;
      if(message.type==='hello'){
        require(!this.hello,'duplicate tile hello');const config=validateTileHello(this.descriptor,message);this.hello=true;this.negotiation=message;
        this._emit('message',new MessageEvent('message',{data:JSON.stringify(message)}));
        if(this.closedOnce)return;this.tileMedia.start(this.transport.datagrams.readable,config);
        this.telemetryTimer=setInterval(()=>{if(this.closedOnce)return;void this.tileMedia.collectStats().then(value=>{this.transportStats.mediaBytes=value.receiver.receivedBytes;}).catch(error=>this._fail(error));},1000);
      }else{require(this.hello,'missing authenticated tile hello');this._emit('message',new MessageEvent('message',{data:JSON.stringify(message)}));}
    }
    this.close();
  }
  _closed(){if(this.closedOnce)return;clearInterval(this.telemetryTimer);this.tileMedia?.close();super._closed();}
}
