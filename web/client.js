import { Decoder } from './decoder.js';
import { VideoDecoderAdapter } from './video-decoder.js';
import { decodeProbe } from './probe.js';
import { AudioPlayer, isAudioPacket } from './audio-player.js';

const keysyms = {Backspace:0xff08,Tab:0xff09,Enter:0xff0d,Escape:0xff1b,Delete:0xffff,Home:0xff50,End:0xff57,PageUp:0xff55,PageDown:0xff56,ArrowLeft:0xff51,ArrowUp:0xff52,ArrowRight:0xff53,ArrowDown:0xff54,Insert:0xff63,Shift:0xffe1,Control:0xffe3,Alt:0xffe9,Meta:0xffeb,CapsLock:0xffe5,NumLock:0xff7f,ScrollLock:0xff14,PrintScreen:0xff61,Pause:0xff13,ContextMenu:0xff67};
for(let n=1;n<=24;n++)keysyms[`F${n}`]=0xffbd+n;

export class GlissClient {
  constructor(canvas, onStatus=()=>{}, onEvent=()=>{}, {url}={}) {
    this.canvas=canvas; this.url=url; this.listeners=new AbortController(); this.ctx=canvas.getContext('2d',{alpha:false}); this.decoder=new Decoder(); this.onStatus=onStatus; this.onEvent=onEvent;
    this.socket=null; this.inputId=0; this.keys=new Map(); this.buttons=0; this.point={x:0,y:0}; this.chain=Promise.resolve(); this.epoch=0;
    this.metrics={frames:0,bytes:0,decodeMs:[],presentTimes:[],forwardedInputAgeMs:[],pixelEchoLatencyMs:[],pixelEchoEvents:[],errors:0};
    this.pendingInputs=new Map(); this.pixelInputs=new Map();
    this.renderStrategy='full';this.codec=null;
    this.profiler=null;this.profilingStart=null;
    this.audio=new AudioPlayer(state=>{
      if(!state.enabled&&this.audioRequested){this.audioRequested=false;this.audioControl(false);}
      this.onEvent(state);
    });this.audioRequested=false;
    this.bindInput();
  }
  async connect() {
    this.disconnect(); this.decoder=new Decoder(); this.chain=Promise.resolve();
    let decoder=this.decoder;const epoch=++this.epoch;
    let socket;
    if(!this.url&&location.pathname==='/datagram'){
      try{
        const {openDatagramSocket}=await import('./tile-connect.js');
        if(epoch!==this.epoch)return;
        socket=await openDatagramSocket({isCurrent:()=>epoch===this.epoch,canvas:this.canvas,onPaint:batch=>this.recordTilePaint(batch,epoch)});
        if(!socket)return;
        if(epoch!==this.epoch){socket.close();return;}
        this.tileMedia=socket.tileMedia??null;
      }catch(error){if(epoch===this.epoch)this.onStatus(`Datagram connection failed: ${error.message}`);return;}
    }else socket=new WebSocket(this.url??`${location.protocol==='https:'?'wss':'ws'}://${location.host}/ws`);
    this.socket=socket; socket.binaryType='arraybuffer';
    socket.onopen=()=>{if(epoch===this.epoch){this.onStatus('Connected');this.canvas.focus();}};
    socket.onclose=event=>{
      if(epoch!==this.epoch)return;
      this.profilingStart=null;this.profiler?.stop('disconnect');
      ++this.epoch;decoder.close?.();this.audioRequested=false;this.audio.dispose();this.keys.clear();this.buttons=0;
      this.pendingInputs.clear();this.pixelInputs.clear();this.onStatus('Desktop disconnected');
      this.onEvent({type:'close',code:event.code,reason:event.reason});
    };
    socket.onerror=()=>{if(epoch===this.epoch)this.onStatus('Connection failed');};
    socket.onmessage=event=>{if(epoch!==this.epoch)return;
      // Audio must keep running when video waits for a decoder or animation frame.
      if(isAudioPacket(event.data)){this.audio.push(event.data);return;}
      const profiling=this.profiler,receivedAt=profiling?performance.now():null;
      if(profiling&&typeof event.data!=='string')profiling.received(event.data.byteLength,receivedAt);
      let textMessage,textError;
      if(typeof event.data==='string'){
        try{textMessage=JSON.parse(event.data);profiling?.control(textMessage,receivedAt);if(textMessage?.type==='audio_status'){this.audio.status(textMessage);this.onEvent(textMessage);return;}if(textMessage?.type==='pong'){this.onEvent(textMessage);return;}}catch(error){textError=error;}
      }
      this.chain=this.chain.then(async()=>{
      if(epoch!==this.epoch)return;
      if(typeof event.data==='string'){
        if(textError)throw textError;
        const message=textMessage;
        if(message.type==='hello'){
          this.codec=message.codec;
          if(message.codec==='h264'){decoder=new VideoDecoderAdapter({timeoutMs:socket.predictiveVideo===true?250:5000,configureTimeoutMs:socket.predictiveVideo===true?250:null});this.decoder=decoder;}
          else if(message.codec==='hybrid'){
            // Negotiate before loading optional media modules, leaving room for
            // the WebSocket on bounded HTTP/1 connection relays.
            const {HybridDecoder}=await import('./hybrid-decoder.js');
            if(epoch!==this.epoch)return;
            decoder=new HybridDecoder();this.decoder=decoder;
          }else if(message.codec==='hybrid-grid'){
            const {HybridGridDecoder}=await import('./hybrid-grid-decoder.js');
            if(epoch!==this.epoch)return;
            decoder=new HybridGridDecoder();this.decoder=decoder;
          }else if(message.codec==='hybrid-continuity'){
            const {HybridContinuityDecoder}=await import('./hybrid-continuity-decoder.js');
            if(epoch!==this.epoch)return;
            decoder=new HybridContinuityDecoder();this.decoder=decoder;
          }
        }
        this.onEvent(message);return;
      }
      const started=performance.now();
      try {
        socket.beginVideoDecode?.(event.data,decoder);
        const frame=await decoder.decode(event.data);
        if(epoch!==this.epoch){frame.videoFrame?.close();return;}
        const decoded=performance.now(),decodeMs=decoded-started;
        this.metrics.decodeMs.push(decodeMs);
        if(!socket.completeVideoDecode&&(this.canvas.width!==frame.width||this.canvas.height!==frame.height)){this.canvas.width=frame.width;this.canvas.height=frame.height;}
        await new Promise(resolve=>requestAnimationFrame(resolve));
        if(epoch!==this.epoch){frame.videoFrame?.close();return;}
        const paintStarted=performance.now();
        if(socket.completeVideoDecode && !socket.completeVideoDecode(frame.sequence,true)){
          frame.videoFrame?.close();return;
        }
        if(socket.completeVideoDecode&&(this.canvas.width!==frame.width||this.canvas.height!==frame.height)){this.canvas.width=frame.width;this.canvas.height=frame.height;}
        if(frame.videoFrame){try{this.ctx.drawImage(frame.videoFrame,0,0);}finally{frame.videoFrame.close();}}
        else if(this.renderStrategy==='damage'){
          const area=frame.damage;
          if(area)this.ctx.putImageData(new ImageData(frame.pixels,frame.width,frame.height),0,0,area.x,area.y,area.width,area.height);
        }else this.ctx.putImageData(new ImageData(frame.pixels,frame.width,frame.height),0,0);
        // This is CPU submission + requestAnimationFrame, not physical photon presentation.
        const presented=performance.now();
        this.presentationState={codec:this.codec,sequence:frame.sequence,width:frame.width,height:frame.height,presentedAtMs:presented,wholeFrame:true};
        (this.metrics.paintMs??=[]).push(presented-paintStarted);
        this.metrics.frames++;this.metrics.bytes+=frame.bytes;this.metrics.presentTimes.push(presented);
        const inputTime=this.pendingInputs.get(frame.inputId);
        if(inputTime!==undefined)this.metrics.forwardedInputAgeMs.push(presented-inputTime);
        for(const id of this.pendingInputs.keys())if(id<=frame.inputId)this.pendingInputs.delete(id);
        const echo=this.pixelInputs.size?this.readProbe():null;
        const pixelTime=this.pixelInputs.get(echo);
        if(pixelTime!==undefined){this.metrics.pixelEchoLatencyMs.push(presented-pixelTime);(this.metrics.pixelEchoEvents??=[]).push({id:echo,latencyMs:presented-pixelTime,presentedAtMs:presented});this.pixelInputs.delete(echo);}
        for(const array of [this.metrics.decodeMs,this.metrics.presentTimes,this.metrics.forwardedInputAgeMs,this.metrics.pixelEchoLatencyMs,this.metrics.pixelEchoEvents??[],this.metrics.paintMs??[]])if(array.length>10000)array.splice(0,array.length-10000);
        socket.send(JSON.stringify({type:'ack',sequence:frame.sequence}));
        profiling?.frame({received:receivedAt,started,decoded,paintStarted,presented,bytes:frame.bytes});
        this.onEvent({type:'frame',frame,decodeMs});
      }catch(error){if(epoch!==this.epoch)return;this.metrics.errors++;decoder.reset();this.onStatus(`Display recovery: ${error.message}`);if(socket.readyState===WebSocket.OPEN)socket.send(JSON.stringify({type:'resync'}));}
    }).catch(error=>{if(epoch===this.epoch){this.metrics.errors++;this.onStatus(error.message);}});};
  }
  recordTilePaint(batch,epoch){
    if(epoch!==this.epoch)return;
    const presented=batch.presented;this.presentationState={codec:this.codec,epoch:batch.epoch,width:batch.width,height:batch.height,presentedAtMs:presented,wholeFrame:false,tiles:batch.tiles};this.metrics.frames++;this.metrics.bytes=this.socket?.transportStats?.mediaBytes??this.metrics.bytes;
    this.metrics.decodeMs.push(batch.decodeMs);this.metrics.presentTimes.push(presented);(this.metrics.paintMs??=[]).push(batch.paintMs);
    this.metrics.tileMedia={...this.tileMedia?.painter.snapshot(),unit:'Actual partial Canvas paint batches; not full-frame FPS.',epoch:batch.epoch};
    const echo=this.pixelInputs.size?this.readProbe():null,pixelTime=this.pixelInputs.get(echo);
    if(pixelTime!==undefined){this.metrics.pixelEchoLatencyMs.push(presented-pixelTime);this.metrics.pixelEchoEvents.push({id:echo,latencyMs:presented-pixelTime,presentedAtMs:presented});this.pixelInputs.delete(echo);}
    for(const array of[this.metrics.decodeMs,this.metrics.presentTimes,this.metrics.paintMs,this.metrics.pixelEchoLatencyMs,this.metrics.pixelEchoEvents])if(array.length>10000)array.splice(0,array.length-10000);
    // Tile headers have no input ID. Forwarded-input age stays unassociated;
    // input-response measurement uses the actual painted probe pixels.
    this.onEvent({type:'frame',frame:{width:batch.width,height:batch.height,epoch:batch.epoch,partial:true,tiles:batch.tiles,bytes:batch.bytes},decodeMs:batch.decodeMs});
  }
  send(input,{pixelEcho=false}={}) {
    if(this.socket?.readyState!==WebSocket.OPEN)return 0;
    const id=this.inputId=(this.inputId+1)>>>0;
    if(this.pendingInputs.size>1024)this.pendingInputs.clear();
    this.pendingInputs.set(id,performance.now());
    if(this.pixelInputs.size>1024)this.pixelInputs.clear();
    if(pixelEcho)this.pixelInputs.set(id&0xffffff,performance.now());
    this.socket.send(JSON.stringify({...input,id}));
    if(input.type==='pointer'){this.point={x:input.x,y:input.y};this.buttons=input.buttons&7;}
    return id;
  }
  audioControl(enabled,codec=this.audio.codec??'pcm'){
    if(this.socket?.readyState===WebSocket.OPEN)this.socket.send(JSON.stringify({type:'audio',enabled,codec}));
  }
  async setAudioEnabled(enabled){
    if(!enabled){const requested=this.audioRequested;this.audioRequested=false;if(requested)this.audioControl(false);this.audio.stop();this.audio.state();return;}
    if(!this.audio.available||this.socket?.readyState!==WebSocket.OPEN)return;
    const socket=this.socket,epoch=this.epoch;
    const codec=await this.audio.enable();
    if(!codec||epoch!==this.epoch||socket!==this.socket||socket.readyState!==WebSocket.OPEN)return;
    this.audioRequested=true;this.audioControl(true,codec);
  }
  async startProfiling(options={}){
    if(this.profiler||this.profilingStart)throw new Error('A profile is already recording');
    const socket=this.socket,epoch=this.epoch;
    if(socket?.readyState!==WebSocket.OPEN||!this.codec)throw new Error('Connect the desktop before profiling');
    if(socket.transportStats||this.tileMedia)throw new Error('Profiling currently supports WebSocket desktops');
    const token={};this.profilingStart=token;
    try{
      const {StreamProfiler}=await import('./profiling.js');
      if(this.profilingStart!==token||this.epoch!==epoch||socket!==this.socket||socket.readyState!==WebSocket.OPEN)throw new Error('Profiling start cancelled');
      const profiler=new StreamProfiler(options,{codec:this.codec,
        sample:()=>({audio:this.audio.snapshot(),bufferedBytes:socket.bufferedAmount}),
        sendProbe:id=>socket.send(JSON.stringify({type:'ping',id})),
        onStop:()=>{if(this.profiler===profiler)this.profiler=null;}});
      this.profiler=profiler;return profiler;
    }finally{if(this.profilingStart===token)this.profilingStart=null;}
  }
  stopProfiling(){this.profilingStart=null;return this.profiler?.stop()??null;}
  readProbe(){return decodeProbe((x,y)=>this.ctx.getImageData(x,y,1,1).data);}
  release() {
    for(const keysym of this.keys.values())this.send({type:'key',keysym,down:false});
    this.keys.clear();const heldButtons=this.buttons;this.buttons=0;
    if(heldButtons)this.send({type:'pointer',...this.point,buttons:0});
  }
  disconnect(){this.profilingStart=null;this.profiler?.stop('disconnect');this.release();this.audioRequested=false;this.audio.dispose();this.socket?.close();this.decoder.close?.();this.pendingInputs.clear();this.pixelInputs.clear();++this.epoch;}
  // Disconnects and removes every listener, for viewers embedded in a longer-lived page.
  dispose(){this.disconnect();this.listeners.abort();}
  bindInput(){
    const canvas=this.canvas,{signal}=this.listeners;
    const coordinates=e=>{const r=canvas.getBoundingClientRect();return{x:Math.max(0,Math.min(canvas.width-1,Math.floor((e.clientX-r.left)*canvas.width/r.width))),y:Math.max(0,Math.min(canvas.height-1,Math.floor((e.clientY-r.top)*canvas.height/r.height)))};};
    const button=e=>[1,4,2][e.button]||0;
    canvas.addEventListener('pointerdown',e=>{e.preventDefault();canvas.focus();canvas.setPointerCapture(e.pointerId);this.point=coordinates(e);this.buttons|=button(e);this.send({type:'pointer',...this.point,buttons:this.buttons});},{signal});
    canvas.addEventListener('pointerup',e=>{e.preventDefault();this.point=coordinates(e);this.buttons&=~button(e);this.send({type:'pointer',...this.point,buttons:this.buttons});},{signal});
    canvas.addEventListener('pointercancel',()=>this.release(),{signal});
    let motion=null,scheduled=false;
    canvas.addEventListener('pointermove',e=>{motion=coordinates(e);if(!scheduled){scheduled=true;requestAnimationFrame(()=>{scheduled=false;if(motion){this.point=motion;motion=null;this.send({type:'pointer',...this.point,buttons:this.buttons});}});}},{signal});
    canvas.addEventListener('contextmenu',e=>e.preventDefault(),{signal});
    canvas.addEventListener('wheel',e=>{e.preventDefault();this.point=coordinates(e);const mask=e.deltaY?e.deltaY<0?8:16:e.deltaX<0?32:64;this.send({type:'pointer',...this.point,buttons:this.buttons|mask});this.send({type:'pointer',...this.point,buttons:this.buttons});},{passive:false,signal});
    canvas.addEventListener('keydown',e=>{e.preventDefault();let symbol=keysyms[e.key];if(e.location===2&&symbol>=0xffe1&&symbol<=0xffeb)symbol++;if(symbol===undefined&&[...e.key].length===1){const code=e.key.codePointAt(0);symbol=code<=255?code:0x01000000|code;}symbol=this.keys.get(e.code)??symbol;if(symbol!==undefined){this.keys.set(e.code,symbol);this.send({type:'key',keysym:symbol,down:true});}},{signal});
    canvas.addEventListener('keyup',e=>{e.preventDefault();const symbol=this.keys.get(e.code);if(symbol!==undefined){this.send({type:'key',keysym:symbol,down:false});this.keys.delete(e.code);}},{signal});
    canvas.addEventListener('blur',()=>this.release(),{signal});
    window.addEventListener('blur',()=>this.release(),{signal});
    document.addEventListener('visibilitychange',()=>{if(document.hidden)this.release();},{signal});
  }
}

