import { ViewerMedia } from './viewer-media.js';

const canvas = document.getElementById('screen');
const context = canvas.getContext('2d', {alpha: false});
const status = document.getElementById('status');
const endpoint = new URL('ws', location.href);
endpoint.protocol = 'ws:';
const socket = new WebSocket(endpoint);
socket.binaryType = 'arraybuffer';
let inputId = 0, buttons = 0, point = {x: 0, y: 0}, capabilities = [];
const held = new Map();
const metrics = {frames: 0, bytes: 0, errors: 0, sequences: [], resets: 0, resyncs: 0,
  hybrid_frames: 0, hybrid_video_frames: 0, hybrid_video_delta_frames: 0, max_pending_media: 0,
  recoveries: 0, recovery_key_sequences: []};

function send(value) {
  if (socket.readyState !== WebSocket.OPEN) return;
  socket.send(JSON.stringify(value));
}
function input(value) {
  if (!capabilities.includes('input')) return;
  send({...value, id: inputId = (inputId + 1) >>> 0});
}
function release() {
  for (const keysym of held.values()) input({type: 'key', keysym, down: false});
  held.clear(); buttons = 0;
  input({type: 'pointer', ...point, buttons: 0});
}
const media = new ViewerMedia({metrics, open:()=>socket.readyState===WebSocket.OPEN, send,
  onError:error=>{status.textContent=error.message;},
  paint:frame=>{
    if(canvas.width!==frame.width||canvas.height!==frame.height){canvas.width=frame.width;canvas.height=frame.height;}
    context.putImageData(new ImageData(frame.pixels,frame.width,frame.height),0,0);
    status.textContent=`${frame.width} × ${frame.height} — ${metrics.frames+1} frames painted`;
  }});
socket.onmessage = event => {
  try{
    if(typeof event.data==='string'){
      const value=JSON.parse(event.data);
      if(value.type==='capabilities'){media.capabilities(value.capabilities);capabilities=value.capabilities;}
      else if(value.type==='hello')media.hello(value);
      else if(value.type==='error'||value.type==='input_error')status.textContent=value.message;
    }else media.enqueue(event.data);
  }catch(error){metrics.errors++;status.textContent=error.message;media.close();socket.close();}
};
socket.onopen = () => canvas.focus();
socket.onclose = () => {media.close();capabilities=[];held.clear();buttons=0;status.textContent='Desktop disconnected';};
socket.onerror = () => {media.close();status.textContent='Connection failed';};

const keysyms = {Enter: 0xff0d, Escape: 0xff1b, Backspace: 0xff08, Tab: 0xff09,
  Shift: 0xffe1, Control: 0xffe3, Alt: 0xffe9, Meta: 0xffeb,
  ArrowLeft: 0xff51, ArrowUp: 0xff52, ArrowRight: 0xff53, ArrowDown: 0xff54,
  Delete: 0xffff, Home: 0xff50, End: 0xff57, PageUp: 0xff55, PageDown: 0xff56};
canvas.addEventListener('keydown', event => {
  event.preventDefault();
  let keysym = held.get(event.code) ?? keysyms[event.key];
  if (keysym === undefined && [...event.key].length === 1) {
    const scalar = event.key.codePointAt(0); keysym = scalar <= 255 ? scalar : 0x01000000 | scalar;
  }
  if (keysym !== undefined) { held.set(event.code, keysym); input({type: 'key', keysym, down: true}); }
});
canvas.addEventListener('keyup', event => {
  event.preventDefault(); const keysym = held.get(event.code);
  if (keysym !== undefined) { input({type: 'key', keysym, down: false}); held.delete(event.code); }
});
function coordinates(event) {
  const rectangle = canvas.getBoundingClientRect();
  return {x: Math.max(0, Math.min(canvas.width - 1, Math.floor((event.clientX - rectangle.left) * canvas.width / rectangle.width))),
          y: Math.max(0, Math.min(canvas.height - 1, Math.floor((event.clientY - rectangle.top) * canvas.height / rectangle.height)))};
}
const mask = event => ({0: 1, 1: 2, 2: 4}[event.button] ?? 0);
canvas.addEventListener('pointerdown', event => {
  event.preventDefault(); canvas.focus(); canvas.setPointerCapture(event.pointerId);
  point = coordinates(event); buttons |= mask(event); input({type: 'pointer', ...point, buttons});
});
canvas.addEventListener('pointerup', event => {
  event.preventDefault(); point = coordinates(event); buttons &= ~mask(event); input({type: 'pointer', ...point, buttons});
});
let motion, scheduled = false;
canvas.addEventListener('pointermove', event => {
  motion = coordinates(event);
  if (!scheduled) { scheduled = true; requestAnimationFrame(() => {
    scheduled = false; if (motion) { point = motion; motion = null; input({type: 'pointer', ...point, buttons}); }
  }); }
});
canvas.addEventListener('contextmenu', event => event.preventDefault());
canvas.addEventListener('wheel', event => {
  event.preventDefault(); point = coordinates(event); const scroll = event.deltaY < 0 ? 8 : 16;
  input({type: 'pointer', ...point, buttons: buttons | scroll}); input({type: 'pointer', ...point, buttons});
}, {passive: false});
canvas.addEventListener('blur', release); window.addEventListener('blur', release);
document.addEventListener('visibilitychange', () => { if (document.hidden) release(); });
document.getElementById('disconnect').onclick = async () => {
  release(); media.close(); socket.close(); await fetch(location.pathname, {method: 'DELETE'});
};
// Observable fixture assertions; no bearer, credential, worker cookie or token.
window.glissPrototype = {metrics, socket, send: input, resync:()=>media.requestRecovery(), pixel: () => [...context.getImageData(0, 0, 1, 1).data]};
