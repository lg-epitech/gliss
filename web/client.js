import { VideoDecoderAdapter } from './video-decoder.js';
import { decodeProbe } from './probe.js';
import { AudioPlayer, isAudioPacket } from './audio-player.js';

const keysyms = {Backspace:0xff08,Tab:0xff09,Enter:0xff0d,Escape:0xff1b,Delete:0xffff,Home:0xff50,End:0xff57,PageUp:0xff55,PageDown:0xff56,ArrowLeft:0xff51,ArrowUp:0xff52,ArrowRight:0xff53,ArrowDown:0xff54,Insert:0xff63,Shift:0xffe1,Control:0xffe3,Alt:0xffe9,Meta:0xffeb,CapsLock:0xffe5,NumLock:0xff7f,ScrollLock:0xff14,PrintScreen:0xff61,Pause:0xff13,ContextMenu:0xff67};
for(let n=1;n<=24;n++)keysyms[`F${n}`]=0xffbd+n;

export class GlissClient {
  constructor(canvas, onStatus=()=>{}, onEvent=()=>{}, {url}={}) {
    this.canvas=canvas; this.url=url; this.listeners=new AbortController(); this.ctx=canvas.getContext('2d',{alpha:false}); this.decoder=new VideoDecoderAdapter(); this.onStatus=onStatus; this.onEvent=onEvent;
    this.socket=null; this.inputId=0; this.keys=new Map(); this.buttons=0; this.point={x:0,y:0}; this.chain=Promise.resolve(); this.epoch=0;
    this.metrics={frames:0,bytes:0,decodeMs:[],presentTimes:[],forwardedInputAgeMs:[],pixelEchoLatencyMs:[],pixelEchoEvents:[],errors:0};
    this.pendingInputs=new Map(); this.pixelInputs=new Map();
    this.codec=null;this.pendingPaint=null;
    this.profiler=null;this.profilingStart=null;
    this.audio=new AudioPlayer(state=>{
      if(!state.enabled&&this.audioRequested){this.audioRequested=false;this.audioControl(false);}
      this.onEvent(state);
    });this.audioRequested=false;
    this.bindInput();
  }
  async connect() {
    this.disconnect(); this.decoder=new VideoDecoderAdapter(); this.chain=Promise.resolve();
    const decoder=this.decoder,epoch=++this.epoch;
    const socket=new WebSocket(this.url??`${location.protocol==='https:'?'wss':'ws'}://${location.host}/ws`);
    this.socket=socket; socket.binaryType='arraybuffer';
    socket.onopen=()=>{if(epoch===this.epoch){this.onStatus('Connected');this.canvas.focus();}};
    socket.onclose=event=>{
      if(epoch!==this.epoch)return;
      ++this.epoch;this.pendingPaint?.();decoder.close();
      this.keys.clear();this.buttons=0;this.pendingInputs.clear();this.pixelInputs.clear();
      this.profilingStart=null;this.profiler?.stop('disconnect');
      this.audioRequested=false;this.audio.dispose();this.onStatus('Desktop disconnected');
      this.onEvent({type:'close',code:event.code,reason:event.reason});
    };
    socket.onerror=()=>{if(epoch===this.epoch)this.onStatus('Connection failed');};
    socket.onmessage=event=>{if(epoch!==this.epoch)return;
      // Audio must keep running when video waits for a decoder or animation frame.
      if(isAudioPacket(event.data)){this.audio.push(event.data);return;}
      const arrived=performance.now(),profiling=this.profiler,receivedAt=profiling?arrived:null;
      const failed=error=>{if(epoch===this.epoch){this.metrics.errors++;this.onStatus(error.message);}};
      if(typeof event.data==='string'){
        // Two messages are about the picture and wait their turn among the frames: hello describes
        // the frames after it, and error says the picture stopped. Every other message is handled
        // now. Behind a frame that waits to be painted in a hidden tab, the guest's cursors and
        // clipboards would otherwise pile up for as long as the tab stays hidden.
        let message;
        try{
          message=JSON.parse(event.data);profiling?.control(message,receivedAt);
          if(message.type==='audio_status')this.audio.status(message);
          if(message.type!=='hello'&&message.type!=='error'){this.onEvent(message);return;}
        }catch(error){failed(error);return;}
        this.chain=this.chain.then(()=>{
          if(epoch!==this.epoch)return;
          if(message.type==='hello')this.codec=message.codec;
          this.onEvent(message);
        }).catch(failed);
        return;
      }
      if(profiling)profiling.received(event.data.byteLength,receivedAt);
      this.chain=this.chain.then(async()=>{
      if(epoch!==this.epoch)return;
      const started=performance.now();let ownedVideoFrame=null;
      try {
        const frame=await decoder.decode(event.data);
        ownedVideoFrame=frame.videoFrame;
        if(epoch!==this.epoch){return;}
        const decoded=performance.now(),decodeMs=decoded-started;
        this.metrics.decodeMs.push(decodeMs);
        if(this.canvas.width!==frame.width||this.canvas.height!==frame.height){this.canvas.width=frame.width;this.canvas.height=frame.height;}
        // Disconnect can release this wait while a hidden document suspends RAF.
        await new Promise(resolve=>{
          let cancel;
          // An old callback can only clear its own cancellation slot.
          const finish=()=>{if(this.pendingPaint===cancel)this.pendingPaint=null;resolve();};
          const id=requestAnimationFrame(finish);
          cancel=()=>{globalThis.cancelAnimationFrame?.(id);finish();};
          this.pendingPaint=cancel;
        });
        if(epoch!==this.epoch){return;}
        const paintStarted=performance.now();
        this.ctx.drawImage(ownedVideoFrame,0,0);
        ownedVideoFrame.close();ownedVideoFrame=null;
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
        // `held` is how long the frame waited here for its decode and a paint. The server
        // takes it off to learn when the frame arrived, which is all the link did.
        socket.send(JSON.stringify({type:'ack',sequence:frame.sequence,held:Math.round(performance.now()-arrived)}));
        profiling?.frame({received:receivedAt,started,decoded,paintStarted,presented,bytes:frame.bytes});
        this.onEvent({type:'frame',frame,decodeMs});
      }catch(error){
        ownedVideoFrame?.close();ownedVideoFrame=null;
        if(epoch!==this.epoch)return;
        this.metrics.errors++;decoder.reset();
        this.onStatus(`Display recovery: ${error.message}`);
        if(socket.readyState===WebSocket.OPEN)socket.send(JSON.stringify({type:'resync'}));
      }finally{ownedVideoFrame?.close();}
    }).catch(failed);};
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
  disconnect(){
    ++this.epoch;this.pendingPaint?.();
    this.profilingStart=null;this.profiler?.stop('disconnect');this.release();this.audioRequested=false;
    this.socket?.close();this.decoder.close();this.pendingInputs.clear();this.pixelInputs.clear();
    this.audio.dispose();
  }
  // Disconnects and removes every listener, for viewers embedded in a longer-lived page.
  dispose(){try{this.disconnect();}finally{this.listeners.abort();}}
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

