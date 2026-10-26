import { spawn } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { isAbsolute, resolve } from 'node:path';
import { createServer } from 'node:net';
import { chromium } from 'playwright';

export const CPU_BROWSER_ARGS=['--disable-gpu','--disable-gpu-compositing','--disable-accelerated-video-decode','--disable-accelerated-video-encode','--use-gl=disabled','--disable-dev-shm-usage','--disable-background-timer-throttling','--disable-renderer-backgrounding'];

export async function launchBrowser({handleSIGINT=true,handleSIGTERM=true}={}){
  const executablePath=process.env.CHROMIUM_PATH;
  return chromium.launch({headless:true,executablePath,args:CPU_BROWSER_ARGS,handleSIGINT,handleSIGTERM});
}

export function serverBinary(){
  const configured=process.env.GLISS_BINARY;
  if(configured&&!isAbsolute(configured))throw new Error('GLISS_BINARY must name an absolute freshly built server');
  const binary=configured||fileURLToPath(new URL('../target/release/gliss-server',import.meta.url));
  if(!existsSync(binary))throw new Error('Build the release server or set GLISS_BINARY to an absolute fresh binary');
  return resolve(binary);
}

export async function freePort(){
  const listener=createServer();
  await new Promise((resolve,reject)=>{listener.once('error',reject);listener.listen(0,'127.0.0.1',resolve);});
  const port=listener.address().port;
  await new Promise(resolve=>listener.close(resolve));
  return port;
}

export function hybridServerArgs(options={}){
  const base=options.hybridBase===undefined?'global':options.hybridBase;
  const profile=options.hybridVideoProfile===undefined?'screen':options.hybridVideoProfile;
  if(!['global','global-cached'].includes(base))throw new Error('Invalid explicit Hybrid base');
  if(!['screen','camera-scene-on','camera-scene-off'].includes(profile))throw new Error('Invalid explicit Hybrid video profile');
  if(!['hybrid','hybrid-grid','hybrid-continuity'].includes(options.codec||'deflate')&&(base!=='global'||profile!=='screen'))throw new Error('Nondefault Hybrid options require a Hybrid codec');
  if(options.hybridMaskGrid!==undefined&&(![16,32].includes(options.hybridMaskGrid)||!['hybrid-grid','hybrid-continuity'].includes(options.codec)))throw new Error('Explicit mask grid16/32 requires hybrid-grid or hybrid-continuity');
  const args=[];
  if(options.hybridBase!==undefined)args.push('--hybrid-base',base);
  if(options.hybridVideoProfile!==undefined)args.push('--hybrid-video-profile',profile);
  if(options.hybridMaskGrid!==undefined)args.push('--hybrid-mask-grid',String(options.hybridMaskGrid));
  return args;
}

export async function startServer(options={}){
  const hybridArgs=hybridServerArgs(options);
  if(options.sharedDbus&&!options.dbusAddress)throw new Error('Shared D-Bus tests require a fixed D-Bus target');
  const port=options.port||await freePort();
  const token=randomBytes(24).toString('base64url');
  const url=`http://127.0.0.1:${port}`;
  const binary=serverBinary();
  const args=['--bind',`127.0.0.1:${port}`,'--codec',options.codec||'deflate','--workload',options.workload||'static','--fps',String(options.fps||60),'--max-clients',String(options.maxClients||8)];
  args.push(...hybridArgs);
  if(options.vnc)args.push('--vnc',options.vnc);else args.push('--demo');
  if(options.origin)args.push('--origin',options.origin);
  if(options.sessionTtl)args.push('--session-ttl',String(options.sessionTtl));
  if(options.h264Bitrate)args.push('--h264-bitrate',String(options.h264Bitrate));
  if(options.inFlightFrames!==undefined)args.push('--in-flight-frames',String(options.inFlightFrames));
  if(options.inFlightBytes!==undefined)args.push('--in-flight-bytes',String(options.inFlightBytes));
  if(options.streamStats)args.push('--stream-stats');
  if(options.dbusAddress){
    if(!options.vnc||!options.vmUuid)throw new Error('D-Bus tests require a fixed VNC input target and expected VM UUID');
    args.push('--dbus-address',options.dbusAddress,'--vm-uuid',options.vmUuid);
    if(options.dbusConsole!==undefined)args.push('--dbus-console',String(options.dbusConsole));
    if(options.sharedDbus)args.push('--shared-dbus');
  }
  const child=spawn(binary,args,{env:{...process.env,GLISS_TOKEN:token},stdio:['ignore','pipe','pipe']});
  let output=''; let error;
  child.on('error',e=>error=e);
  for(const stream of [child.stdout,child.stderr])stream.on('data',chunk=>{output=(output+chunk.toString()).slice(-8192);});
  const stop=async()=>{if(child.exitCode!==null)return;child.kill('SIGINT');await Promise.race([new Promise(resolve=>child.once('exit',resolve)),new Promise(resolve=>setTimeout(()=>{child.kill('SIGKILL');resolve();},2000).unref())]);};
  const deadline=Date.now()+15000;
  while(Date.now()<deadline){
    if(error||child.exitCode!==null){await stop();throw error||new Error(`Server exited ${child.exitCode}: ${output}`);}
    try{if((await fetch(`${url}/health`)).ok)return {url,port,token,child,stop,output:()=>output};}catch{}
    await new Promise(resolve=>setTimeout(resolve,30));
  }
  await stop();throw new Error(`Server startup timeout: ${output}`);
}

export async function loginPage(page,server,url=server.url){
  await page.goto(url);
  await page.locator('#token').fill(server.token);
  await page.locator('#login button').click();
  await page.waitForFunction(()=>window.gliss?.metrics.frames>0,null,{timeout:15000});
}
