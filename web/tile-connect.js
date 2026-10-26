import {DatagramSocket} from './datagram-transport.js';

// Fetch exactly one one-use ticket. Optional tile modules load only after this
// authenticated descriptor selects them; cancelled connects acquire no media.
export async function openDatagramSocket({isCurrent=()=>true,canvas,onPaint,loadTiles=()=>import('./tile-transport.js')}={}){
  const response=await fetch('/api/datagram-ticket',{method:'POST',credentials:'same-origin',cache:'no-store'});
  if(!response.ok)throw new Error('The datagram session was rejected');const descriptor=await response.json();if(!isCurrent())return null;
  const codec=descriptor.datagram_codec??descriptor.codec??'h264';
  if(codec==='h264')return new DatagramSocket({descriptor});
  if(['h264-predictive-gop4','h264-predictive-gop4-head100','h264-predictive-gop4-compact100','h264-predictive-gop4-compact50','h264-predictive-gop4-compact50-xor8'].includes(codec)){
    const {PredictiveDatagramSocket}=await import('./predictive-datagram.js');if(!isCurrent())return null;
    return new PredictiveDatagramSocket({descriptor});
  }
  if(!['glt2-lossless','glt2-gentle','glt2-strong'].includes(codec))throw new Error('Unsupported authenticated datagram codec');
  const {TileDatagramSocket}=await loadTiles();if(!isCurrent())return null;
  return new TileDatagramSocket({descriptor,canvas,onPaint});
}
