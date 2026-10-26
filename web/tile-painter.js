import {newer,tileCount,tileRect,MAX_TILES,MAX_COMPLETE_BYTES,MAX_AGE_MS} from './tile-decoder.js';
const require=(ok,message)=>{if(!ok)throw new Error(message);};
const u32=value=>Number.isInteger(value)&&value>=0&&value<=0xffffffff;
// The retained desktop image is separate from the existing 4 MiB delta queue.
// This bounds its CPU pixel storage to 33,177,600 bytes at maximum geometry.
const MAX_BACKING_BYTES=3840*2160*4;

// Queue acceptance releases the worker transfer slot. Only putImageData below
// updates painted state. Input/control never waits for this queue or its rAF.
export class TilePainter{
  constructor(canvas,{onPaint=()=>{},onError=error=>{throw error;},now=()=>performance.now(),requestFrame=fn=>requestAnimationFrame(fn),cancelFrame=id=>cancelAnimationFrame(id)}={}){
    this.canvas=canvas;this.context=canvas.getContext('2d',{alpha:false});require(this.context,'CPU canvas unavailable');this.onPaint=onPaint;this.onError=onError;this.now=now;this.requestFrame=requestFrame;this.cancelFrame=cancelFrame;
    this.queue=new Map();this.bytes=0;this.cursor=0;this.pendingEpoch=null;this.pendingWidth=0;this.pendingHeight=0;this.epoch=null;this.width=0;this.height=0;this.image=null;this.painted=[];this.readyCount=0;this.frameHandle=null;this.closed=false;
    this.stats={acceptedBatches:0,acceptedTiles:0,replaced:0,evicted:0,expired:0,stale:0,partialPaintBatches:0,paintedTiles:0,paintedBytes:0,queueBytesPeak:0,queueCountPeak:0,paintMs:0,epochCommits:0,backingBytesPeak:0,visibleCalls:0,visibleDirtyBytes:0};
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
    let image=this.image,epoch=this.epoch,width=this.width,height=this.height;
    let left=Infinity,top=Infinity,right=0,bottom=0;
    // Consume the already bounded newest-per-tile queue in this browser tick.
    // Missing or not-yet-decoded tiles never delay eligible independent tiles.
    for(let visited=0;visited<MAX_TILES;visited++){
      const tile=this.cursor;this.cursor=(this.cursor+1)%MAX_TILES;const patch=this.queue.get(tile);if(!patch)continue;this._remove(tile);
      if(this.now()-patch.arrival>=MAX_AGE_MS){this.stats.expired++;continue;}
      const old=this.epoch===patch.epoch?this.painted[tile]:null;if(old&&!newer(patch.sequence,old.sequence)){this.stats.stale++;continue;}
      if(epoch!==patch.epoch){
        width=patch.frameWidth;height=patch.frameHeight;epoch=patch.epoch;
        const length=width*height*4;require(length>0&&length<=MAX_BACKING_BYTES,'CPU desktop image allocation bound');
        // Release the previous retained image before allocating new geometry.
        this.image=null;image=null;const pixels=new Uint8ClampedArray(length);
        for(let offset=3;offset<length;offset+=4)pixels[offset]=255;
        image=new ImageData(pixels,width,height);this.stats.backingBytesPeak=Math.max(this.stats.backingBytesPeak,length);
      }
      require(image&&image.width===patch.frameWidth&&image.height===patch.frameHeight,'CPU desktop image geometry conflict');
      const rowBytes=patch.width*4;
      for(let row=0;row<patch.height;row++)image.data.set(patch.pixels.subarray(row*rowBytes,(row+1)*rowBytes),((patch.y+row)*width+patch.x)*4);
      left=Math.min(left,patch.x);top=Math.min(top,patch.y);right=Math.max(right,patch.x+patch.width);bottom=Math.max(bottom,patch.y+patch.height);
      patches.push({epoch:patch.epoch,tile,sequence:patch.sequence,arrival:patch.arrival,decoded:patch.decoded});
      bytes+=patch.pixels.byteLength;decodeMs+=patch.decodeMs??0;require(bytes<=MAX_COMPLETE_BYTES,'CPU paint delta bound');
    }
    if(patches.length){
      const changedEpoch=this.epoch!==epoch;
      if(changedEpoch){this.canvas.width=width;this.canvas.height=height;}
      // One visible write presents every eligible queued patch. This does not
      // claim capture atomicity when tiles arrive in different browser ticks.
      this.context.putImageData(image,0,0,left,top,right-left,bottom-top);
      const painted=this.now(),paintMs=painted-started;this.image=image;
      if(changedEpoch){this.epoch=epoch;this.width=width;this.height=height;this.painted=Array(tileCount(width,height)).fill(null);this.readyCount=0;this.stats.epochCommits++;}
      // Only the successful visible call commits coverage and tile stamps.
      for(const stamp of patches){if(!this.painted[stamp.tile])this.readyCount++;stamp.painted=painted;this.painted[stamp.tile]=stamp;}
      this.stats.partialPaintBatches++;this.stats.paintedTiles+=patches.length;this.stats.paintedBytes+=bytes;this.stats.paintMs+=paintMs;this.stats.visibleCalls++;this.stats.visibleDirtyBytes+=(right-left)*(bottom-top)*4;
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
  snapshot(){return{...this.stats,paintedEpoch:this.epoch,width:this.width,height:this.height,readyTiles:this.readyCount,totalTiles:this.painted.length,completeEpochCoverage:this.painted.length>0&&this.readyCount===this.painted.length,queueBytes:this.bytes,queueCount:this.queue.size,backingBytes:this.image?.data.byteLength??0,backingBytesLimit:MAX_BACKING_BYTES};}
  close(){this.closed=true;if(this.frameHandle!==null)this.cancelFrame(this.frameHandle);this.frameHandle=null;for(const tile of this.queue.keys())this._remove(tile);this.image=null;}
}
