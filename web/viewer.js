// Standalone viewer page served at / and /datagram.
import { GlissClient } from './client.js';

const $=id=>document.getElementById(id);
let cliptext='';let sourceType='';
const client=new GlissClient($('screen'),text=>$('status').textContent=text,event=>{
  if(event.type==='hello')sourceType=event.source;
  if(event.type==='frame'){
    $('resolution').textContent=`${event.frame.width} × ${event.frame.height}`;
    $('traffic').textContent=`${(client.metrics.bytes/1048576).toFixed(2)} MiB received`;
    const ts=client.metrics.presentTimes;const tail=ts.slice(-60);const fps=tail.length>1?(tail.length-1)*1000/(tail.at(-1)-tail[0]):0;
    $('rate').textContent=`${fps.toFixed(1)} updates/s`;
    $('decode').textContent=`Decode ${event.decodeMs.toFixed(1)} ms`;
  }
  if(event.type==='clipboard'){cliptext=event.text;$('cliptext').value=cliptext;}
  if(event.type==='cursor'){
    if(!event.width||!event.height){$('screen').style.cursor='none';return;}
    if(event.width<=128&&event.height<=128&&event.pixels.length===event.width*event.height*4){const c=document.createElement('canvas');c.width=event.width;c.height=event.height;c.getContext('2d').putImageData(new ImageData(new Uint8ClampedArray(event.pixels),c.width,c.height),0,0);$('screen').style.cursor=`url(${c.toDataURL()}) ${event.hotspot_x} ${event.hotspot_y}, default`;}
  }
  if(event.type==='error'||event.type==='input_error')$('status').textContent=event.message;
  if(event.type==='resize_result'&&event.status!==0)$('status').textContent='The VM declined the desktop size change.';
  if(event.type==='display_state')$('status').textContent=event.available?'Connected':'The VM display is temporarily unavailable.';
});
window.gliss=client;
window.glissSource=()=>sourceType;

$('login').addEventListener('submit',async e=>{
  e.preventDefault();$('error').textContent='';
  try{const response=await fetch('/api/session',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({token:$('token').value})});if(!response.ok)throw new Error(response.status===429?'Too many attempts. Try again in a minute.':'The access key was rejected.');$('token').value='';$('login').hidden=true;$('desktop').style.display='block';for(const id of ['clipboard','fit','fullscreen','disconnect'])$(id).hidden=false;client.connect();}catch(error){$('error').textContent=error.message;}
});
$('disconnect').onclick=async()=>{client.disconnect();await fetch('/api/session',{method:'DELETE'});$('desktop').style.display='none';$('login').hidden=false;for(const id of ['clipboard','fit','fullscreen','disconnect'])$(id).hidden=true;};
$('clipboard').onclick=()=>{$('cliptext').value=cliptext;$('clipdialog').showModal();};
$('sendclip').onclick=()=>{client.send({type:'clipboard',text:$('cliptext').value});$('clipdialog').close();};
$('fit').onclick=()=>{const box=$('viewport').getBoundingClientRect();let width=Math.max(320,Math.min(client.codec==='h264'||client.codec?.startsWith('glt2-')?3840:4096,Math.round(box.width-2))),height=Math.max(200,Math.min(2160,Math.round(window.innerHeight-box.top-80)));if(client.codec==='h264'){width-=width%2;height-=height%2;}client.send({type:'resize',width,height});};
$('fullscreen').onclick=()=>{if(document.fullscreenElement)document.exitFullscreen();else $('viewport').requestFullscreen();};
