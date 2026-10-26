import {readdir} from 'node:fs/promises';
import {fileURLToPath} from 'node:url';
import {spawn} from 'node:child_process';

const root=fileURLToPath(new URL('../',import.meta.url));
const tests=(await readdir(new URL('../tests/',import.meta.url)))
  .filter(name=>name.endsWith('.test.mjs')).sort();
const excluded=['owned-qmp-fd.test.mjs','qemu.test.mjs','dbus-qemu.test.mjs'];
if(tests.some(name=>excluded.includes(name)))throw new Error('Prepared-VM tests must stay outside the portable release suite');
console.error(JSON.stringify({kind:'gliss-portable-test-scope',requires_prepared_vm:false,
  included:tests,additional:['third_party/fflate/inflate.test.mjs'],
  excluded:excluded.map(name=>({name,status:'not run',reason:'Prepared-QEMU/KVM integration suite'})),
  native_tile_fixture:'Included retained twelve-step packet/RGBA golden unless explicitly overridden'}));
const files=[...tests.map(name=>fileURLToPath(new URL('../tests/'+name,import.meta.url))),
  fileURLToPath(new URL('../third_party/fflate/inflate.test.mjs',import.meta.url))];
const child=spawn(process.execPath,['--test','--test-concurrency=1',...files],{cwd:root,stdio:'inherit'});
const forward=signal=>{if(child.exitCode===null&&child.signalCode===null)child.kill(signal);};
const interrupt=()=>forward('SIGINT'),terminate=()=>forward('SIGTERM');
process.on('SIGINT',interrupt);process.on('SIGTERM',terminate);
child.once('error',error=>{console.error(error.message);process.exitCode=1;});
child.once('close',(code,signal)=>{
  process.off('SIGINT',interrupt);process.off('SIGTERM',terminate);
  if(signal)console.error('Portable test runner terminated by '+signal);
  process.exitCode=code??1;
});
