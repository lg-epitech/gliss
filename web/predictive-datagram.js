// Explicit GOP4 transport: one decoder token, no latest-P replacement.
import {DatagramSocket,controlMessages} from './datagram-transport.js';
import {PredictiveReceiver} from './predictive-state.js';
import {FecPredictiveReceiver} from './fec-predictive-state.js';
import {MAGIC,parseDatagram} from './xor-fec.js';
const require=(value,message)=>{if(!value)throw new Error(message);};
const FEC_FIELDS=['fec','fec_scope','fec_data_fragments_per_group','fec_parity_fragments_per_group','parity_datagram_protocol'];
export function requirePredictiveCapability(value){
  const interval=value?.datagram_codec==='h264-predictive-gop4'?1000:
    ['h264-predictive-gop4-head100','h264-predictive-gop4-compact100'].includes(value?.datagram_codec)?100:
    ['h264-predictive-gop4-compact50','h264-predictive-gop4-compact50-xor8'].includes(value?.datagram_codec)?50:null;
  require(interval!==null && value.media==='GLD1/GLV1-predictive-v1' &&
    value.protocol==='GLV1' && value.datagram_protocol==='GLD1' && value.independent_frames===false &&
    value.media_retransmission===false && value.reference_policy==='contiguous-since-idr' && value.gop_frames===4 &&
    value.maximum_access_unit_bytes===1048576 && value.encoded_head_interval_ms===interval &&
    value.encoded_head_deadline_ms===250 && value.recovery_interval_ms===1000,'Unsupported predictive datagram capability');
  if(['h264-predictive-gop4-compact100','h264-predictive-gop4-compact50','h264-predictive-gop4-compact50-xor8'].includes(value.datagram_codec)){
    require(value.encoded_head_notification==='on-change-coalesced-v1'&&value.statistics_interval_ms===1000&&
      value.encoded_head_max_body_bytes===128,'Unsupported compact predictive head');
  }
  if(value.datagram_codec==='h264-predictive-gop4-compact50-xor8'){
    require(value.fec==='xor-grouped-v1'&&value.fec_scope==='all-access-units'&&value.fec_data_fragments_per_group===8&&
      value.fec_parity_fragments_per_group===1&&value.parity_datagram_protocol==='GLP1','Unsupported predictive FEC mode');
  }else require(FEC_FIELDS.every(key=>!Object.hasOwn(value,key)),'Unexpected predictive FEC mode');
  return interval;
}
export class PredictiveDatagramSocket extends DatagramSocket {
  constructor({descriptor}){
    const headInterval=requirePredictiveCapability(descriptor);super({descriptor});
    this.headInterval=headInterval;this.profile=descriptor.datagram_codec;
    this.compactHead=['h264-predictive-gop4-compact100','h264-predictive-gop4-compact50','h264-predictive-gop4-compact50-xor8'].includes(this.profile);
    this.fecEnabled=this.profile==='h264-predictive-gop4-compact50-xor8';
    this.predictiveVideo=true;
    clearInterval(this.expiryTimer);this.receiver=this.fecEnabled?new FecPredictiveReceiver():new PredictiveReceiver();this.selected=null;
    this.transportStats={presented:0,mediaBytes:0,reliableBytes:0,receivedDatagramBytes:0,
      dataDatagrams:0,dataDatagramBytes:0,parityDatagrams:0,parityDatagramBytes:0,
      forwardedDataDatagrams:0,forwardedDataDatagramBytes:0,reconstructedFragments:0,
      recoveryEnqueued:0,recoveryWriteCompleted:0,decodedOutputSuppressed:0,
      mediaBytesScope:'Complete gate-admitted GLV1 access units; no wire-byte claim.',
      receivedDatagramBytesScope:'All WebTransport datagram bytes presented to the receiver.',
      fecWireBytesScope:'Canonical received GLD1/GLP1 datagram bytes including their 20-byte application headers.',
      forwardedDataBytesScope:'Canonical original and reconstructed GLD1 bytes forwarded to assembly, including 20-byte application headers.'};
    if(this.compactHead)this.transportStats.compactHeadMessagesReceived=0;
    this.expiryTimer=setInterval(()=>{try{this._pump();}catch(error){this._fail(error);}},8);
  }
  async _readControl(readable){
    for await(const message of controlMessages(readable)){
      if(this.closedOnce)return;
      if(message.type==='hello'){
        require(!this.hello && message.transport==='webtransport-datagram' && message.codec==='h264','Invalid predictive hello');
        require(message.datagram_codec===this.profile&&requirePredictiveCapability(message)===this.headInterval&&
          ['h264-predictive-gop4-compact100','h264-predictive-gop4-compact50','h264-predictive-gop4-compact50-xor8'].includes(message.datagram_codec)===Boolean(this.compactHead),
          'Predictive head profile changed');this.hello=true;
      }else require(this.hello,'Missing predictive hello');
      if(message.type==='encoded_head'){
        require(this.compactHead===true&&Object.keys(message).sort().join(',')==='encoded_head_interval_ms,latest_encoded_sequence,type'&&
          message.encoded_head_interval_ms===this.headInterval&&Number.isInteger(message.latest_encoded_sequence)&&
          message.latest_encoded_sequence>=0&&message.latest_encoded_sequence<=0xffffffff,'Invalid compact encoded head');
        this.receiver.observeHead(message.latest_encoded_sequence,performance.now());
        this.transportStats.compactHeadMessagesReceived++;
      }
      if(message.type==='stream_stats'){
        if(this.compactHead)require(message.statistics_interval_ms===1000,'Invalid compact full-statistics cadence');
        const head=message.latest_encoded_sequence;
        require(message.encoded_head_interval_ms===this.headInterval && (head===null || (Number.isInteger(head)&&head>=0&&head<=0xffffffff)),
          'Invalid reliable encoded head');
        if(head!==null)this.receiver.observeHead(head,performance.now());
      }
      this._emit('message',new MessageEvent('message',{data:JSON.stringify(message)}));this._pump();
    }
    this.close();
  }
  async _readMedia(){
    const reader=this.transport.datagrams.readable.getReader();
    try{for(;;){
      const {value,done}=await reader.read();if(done||this.closedOnce)return;
      require(value instanceof Uint8Array,'Actual predictive datagram bytes');this.transportStats.receivedDatagramBytes+=value.byteLength;
      let packet=null;try{packet=parseDatagram(value);}catch{}
      const magic=value.byteLength>=4?new DataView(value.buffer,value.byteOffset,value.byteLength).getUint32(0,true):null;
      require(this.fecEnabled||magic!==MAGIC.parity,'GLP1 requires negotiated predictive FEC');
      const before=this.receiver.acceptedBytes;this.receiver.receive(value,performance.now());
      const admitted=this.receiver.acceptedBytes-before;require(Number.isSafeInteger(admitted)&&admitted>=0,'Monotonic admitted media byte count');
      this.transportStats.mediaBytes+=admitted;
      if(this.fecEnabled){
        const stats=this.receiver.stats;
        this.transportStats.dataDatagrams=stats.receivedDataDatagrams;this.transportStats.dataDatagramBytes=stats.receivedDataBytes;
        this.transportStats.parityDatagrams=stats.receivedParityDatagrams;this.transportStats.parityDatagramBytes=stats.receivedParityBytes;
        this.transportStats.forwardedDataDatagrams=stats.forwardedDataDatagrams;
        this.transportStats.forwardedDataDatagramBytes=stats.forwardedDataBytes;
        this.transportStats.reconstructedFragments=stats.recoveredFragments;
      }else if(packet?.kind==='data'){
        this.transportStats.dataDatagrams++;this.transportStats.dataDatagramBytes+=value.byteLength;
        this.transportStats.forwardedDataDatagrams++;this.transportStats.forwardedDataDatagramBytes+=value.byteLength;
      }
      this._pump();
    }}finally{reader.releaseLock();}
  }
  _deliverLatest(){this._pump();}
  _pump(){
    if(this.closedOnce||!this.hello)return;
    const next=this.receiver.next(performance.now());
    if(next){
      require(!this.selected,'One actual decoder token');this.selected=next;this.delivering=true;
      this._emit('message',new MessageEvent('message',{data:next.buffer}));
    }
    this._requestRecovery();
  }
  _requestRecovery(){
    if(this.closedOnce||!this.hello)return;
    const status=this.receiver.snapshot().reference;
    if(status.lastDecodedSequence===null && !status.knownLoss && !status.malformed && !status.awaitingIdr &&
      !status.decodeFailed && !status.headTimeouts && !status.expired && !status.capacityDrops)return;
    const request=this.receiver.recovery(performance.now());if(!request)return;
    super.send(JSON.stringify(request));if(this.closedOnce)return;
    this.transportStats.recoveryEnqueued++;
    this.writes.then(()=>{if(!this.closedOnce)this.transportStats.recoveryWriteCompleted++;});
  }
  beginVideoDecode(buffer,decoder){
    require(this.selected?.buffer===buffer,'Decode only the gate-selected actual access unit');
    if(this.selected.resetDecoder)decoder.reset();
  }
  completeVideoDecode(sequence,ok){
    require(this.selected?.sequence===sequence,'Complete the exact actual decoder token');
    const completion=this.receiver.complete(this.selected.token,ok,performance.now());this.selected=null;this.delivering=false;
    if(!completion.paintEligible)this.transportStats.decodedOutputSuppressed++;
    this.transportStats.predictive=this.receiver.snapshot();this._pump();return completion.paintEligible;
  }
  send(text){
    const message=JSON.parse(text);
    if(message.type==='ack'){this.transportStats.presented++;return;}
    if(message.type==='resync'){
      if(this.selected)this.completeVideoDecode(this.selected.sequence,false);
      else this.receiver.loss(null,performance.now());
      this._requestRecovery();return;
    }
    super.send(text);
  }
  predictiveSnapshot(){return this.receiver?.snapshot()??null;}
  _closed(){
    if(this.closedOnce)return;const receiver=this.receiver;this.receiver=null;this.selected=null;this.delivering=false;
    receiver?.close();super._closed();
  }
}
