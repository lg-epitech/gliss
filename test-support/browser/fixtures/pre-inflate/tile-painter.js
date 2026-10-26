import {newer,tileCount,tileRect,MAX_TILES,MAX_COMPLETE_BYTES,MAX_AGE_MS} from './tile-decoder.js';
const require=(ok,message)=>{if(!ok)throw new Error(message);};
const u32=value=>Number.isInteger(value)&&value>=0&&value<=0xffffffff;

// Queue acceptance releases the worker transfer slot. Only putImageData below
// updates painted state. Input/control never waits for this queue or its rAF.
export class TilePainter{
  constructor(canvas,{onPaint=()=>{},onError=error=>{throw error;},now=()=>performance.now(),requestFrame=fn=>requestAnimationFrame(fn),cancelFrame=id=>cancelAnimationFrame(id)}={}){
    this.canvas=canvas;this.context=canvas.getContext('2d',{alpha:false});require(this.context,'CPU canvas unavailable');this.onPaint=onPaint;this.onError=onError;this.now=now;this.requestFrame=requestFrame;this.cancelFrame=cancelFrame;
    this.queue=new Map();this.bytes=0;this.cursor=0;this.pendingEpoch=null;this.pendingWidth=0;this.pendingHeight=0;this.epoch=null;this.width=0;this.height=0;this.painted=[];this.readyCount=0;this.frameHandle=null;this.closed=false;
    this.stats={acceptedBatches:0,acceptedTiles:0,replaced:0,evicted:0,expired:0,stale:0,partialPaintBatches:0,paintedTiles:0,paintedBytes:0,queueBytesPeak:0,queueCountPeak:0,paintMs:0,epochCommits:0};
  }
  _remove(tile){const patch=this.queue.get(tile);if(patch){this.queue.delete(tile);this.bytes-=patch.pixels.byteLength;}return patch;}
  acceptBatch(patches){
    require(!this.closed&&Array.isArray(patches)&&patches.length>0&&patches.length<=64,'invalid tile batch');let bytes=0;const first=patches[0],seen=new Set();
    for(const patch of patches){require(u32(patch.epoch)&&u32(patch.sequence)&&Number.isInteger(patch.frameWidth)&&patch.frameWidth>0&&patch.frameWidth<=3840&&Number.isInteger(patch.frameHeight)&&patch.frameHeight>0&&patch.frameHeight<=2160,'invalid patch geometry/identity');
      require(patch.epoch===first.epoch&&patch.frameWidth===first.frameWidth&&patch.frameHeight===first.frameHeight,'mixed batch epoch/geometry');
      require(Number.isInteger(patch.tile)&&patch.tile>=0&&patch.tile<tileCount(patch.frameWidth,patch.frameHeight)&&!seen.has(patch.tile),'invalid/repeated patch tile');seen.add(patch.tile);
      const r=tileRect(patch.frameWidth,patch.frameHeight,patch.tile);require(['x','y','width','height'].every(field=>patch[field]===r[field]),'invalid patch rectangle');
      require(patch.pixels instanceof Uint8ClampedArray&&patch.pixels.byteLength===r.width*r.height*4&&Number.isFinite(patch.arrival)&&Number.isFinite(patch.decoded),'invalid patch storage/time');bytes+=patch.pixels.byteLength;
    }
    require(bytes<=262144,'transfer pixel byte bound');
    if(this.pendingEpoch!==null&&first.epoch!==this.pendingEpoch&&!newer(first.epoch,this.pendingEpoch)){this.stats.stale+=patches.length;return;}
    if(this.pendingEpoch===first.epoch)require(this.pendingWidth===first.frameWidth&&this.pendingHeight===first.frameHeight,'epoch geometry changed');
    if(this.pendingEpoch!==first.epoch){for(const tile of this.queue.keys())this._remove(tile);this.pendingEpoch=first.epoch;this.pendingWidth=first.frameWidth;this.pendingHeight=first.frameHeight;}
    this.stats.acceptedBatches++;
    for(const patch of patches){
      if(this.now()-patch.arrival>=MAX_AGE_MS){this.stats.expired++;continue;}
      const painted=this.epoch===patch.epoch?this.painted[patch.tile]:null,old=this.queue.get(patch.tile);
      if(painted&&!newer(patch.sequence,painted.sequence)||old&&!newer(patch.sequence,old.sequence)){this.stats.stale++;continue;}
      if(old){this._remove(patch.tile);this.stats.replaced++;}
      while(this.bytes+patch.pixels.byteLength>MAX_COMPLETE_BYTES||this.queue.size>=MAX_TILES){let victim=null,oldest=Infinity;for(const[tile,value]of this.queue)if(value.arrival<oldest){victim=tile;oldest=value.arrival;}require(victim!==null,'paint queue allocation bound');this._remove(victim);this.stats.evicted++;}
      this.queue.set(patch.tile,patch);this.bytes+=patch.pixels.byteLength;this.stats.acceptedTiles++;this.stats.queueBytesPeak=Math.max(this.stats.queueBytesPeak,this.bytes);this.stats.queueCountPeak=Math.max(this.stats.queueCountPeak,this.queue.size);
    }
    this._schedule();
  }
  _schedule(){if(!this.closed&&this.queue.size&&this.frameHandle===null)this.frameHandle=this.requestFrame(()=>{this.frameHandle=null;try{this.paint();}catch(error){this.close();this.onError(error);}});}
  paint(){
    if(this.closed)return;const started=this.now(),patches=[];let bytes=0,decodeMs=0;
    for(let visited=0;visited<MAX_TILES&&patches.length<256;visited++){
      if(patches.length&&this.now()-started>=4)break;const tile=this.cursor;this.cursor=(this.cursor+1)%MAX_TILES;const patch=this.queue.get(tile);if(!patch)continue;this._remove(tile);
      if(this.now()-patch.arrival>=MAX_AGE_MS){this.stats.expired++;continue;}
      if(this.epoch!==patch.epoch){this.canvas.width=patch.frameWidth;this.canvas.height=patch.frameHeight;this.epoch=patch.epoch;this.width=patch.frameWidth;this.height=patch.frameHeight;this.painted=Array(tileCount(this.width,this.height)).fill(null);this.readyCount=0;this.stats.epochCommits++;}
      const old=this.painted[tile];if(old&&!newer(patch.sequence,old.sequence)){this.stats.stale++;continue;}
      this.context.putImageData(new ImageData(patch.pixels,patch.width,patch.height),patch.x,patch.y);
      const painted=this.now();if(!old)this.readyCount++;const stamp={epoch:patch.epoch,tile,sequence:patch.sequence,arrival:patch.arrival,decoded:patch.decoded,painted};this.painted[tile]=stamp;
      patches.push(stamp);bytes+=patch.pixels.byteLength;decodeMs+=patch.decodeMs??0;
    }
    if(patches.length){const painted=this.now(),paintMs=painted-started;this.stats.partialPaintBatches++;this.stats.paintedTiles+=patches.length;this.stats.paintedBytes+=bytes;this.stats.paintMs+=paintMs;
      this.onPaint({width:this.width,height:this.height,epoch:this.epoch,patches,tiles:patches.length,bytes,decodeMs,paintMs,presented:painted,completeEpochCoverage:this.readyCount===this.painted.length,coverage:this.readyCount/this.painted.length});
    }
    this._schedule();
  }
  snapshotRegion({x,y,width,height}){
    require([x,y,width,height].every(Number.isInteger)&&x>=0&&y>=0&&width>0&&height>0,'invalid ROI');
    const now=this.now(),result={epoch:this.epoch,width:this.width,height:this.height,snapshotAt:now,ready:false,tiles:[]};
    if(this.epoch===null||x+width>this.width||y+height>this.height)return result;
    const columns=Math.ceil(this.width/32);for(let row=Math.floor(y/32);row<=Math.floor((y+height-1)/32);row++)for(let col=Math.floor(x/32);col<=Math.floor((x+width-1)/32);col++){
      const tile=row*columns+col,stamp=this.painted[tile];result.tiles.push(stamp?{...stamp,ready:true,localArrivalAgeMs:now-stamp.arrival,localDecodeAgeMs:now-stamp.decoded,localPaintAgeMs:now-stamp.painted}:{tile,epoch:this.epoch,sequence:null,ready:false});
    }
    result.ready=result.tiles.every(tile=>tile.ready);return result;
  }
  snapshot(){return{...this.stats,paintedEpoch:this.epoch,width:this.width,height:this.height,readyTiles:this.readyCount,totalTiles:this.painted.length,completeEpochCoverage:this.painted.length>0&&this.readyCount===this.painted.length,queueBytes:this.bytes,queueCount:this.queue.size};}
  close(){this.closed=true;if(this.frameHandle!==null)this.cancelFrame(this.frameHandle);this.frameHandle=null;for(const tile of this.queue.keys())this._remove(tile);}
}
