"""Retrospective waveform check of the already-retained320ms stereo Opus fixtures.
No encoding, browser, device, network or physical playback. Not a music-quality gate.
"""
import ctypes as C
import gzip
import hashlib
import json
import math
import struct
import sys
from pathlib import Path
HERE=Path(__file__).resolve().parent
ROOT=HERE.parents[1]
def digest(raw):return hashlib.sha256(raw).hexdigest()
def rust_round(x):return math.floor(x+.5) if x>=0 else math.ceil(x-.5)
def metrics(reference,decoded):
    if len(reference)!=len(decoded) or not reference:raise ValueError('sample length')
    error=[float(b)*32768-a for a,b in zip(reference,decoded)]
    squared=math.fsum(x*x for x in error);power=math.fsum(float(x)*x for x in reference)
    return {'samples':len(reference),'mae_s16_units':math.fsum(abs(x) for x in error)/len(error),'rmse_s16_units':math.sqrt(squared/len(error)),
      'snr_db':10*math.log10(power/squared) if squared and power else None,'exact':squared==0}
def check_wire(packet,frames,sequence):
    if not 21<=len(packet)<=1295 or bytes(packet[:12])!=b'GLA1\x01\x02\0\0\x80\xbb\0\0' or struct.unpack('<II',bytes(packet[12:20]))!=(sequence,frames):raise ValueError('GLA1 fixture header')
    return bytes(packet[20:])
def main():
    if len(sys.argv)!=3:raise ValueError('supply absolute libopus and fresh output directory')
    library=Path(sys.argv[1]).resolve(strict=True);out=Path(sys.argv[2]);out.mkdir(mode=0o700,exist_ok=False)
    plan=json.loads((HERE/'quality-plan.json').read_bytes());native_packed=(ROOT/plan['native_file']).read_bytes()
    if digest(native_packed)!=plan['native_compressed_sha256'] or digest(library.read_bytes())!=plan['decoder_library_sha256']:raise ValueError('dependency drift')
    native=json.loads(gzip.decompress(native_packed))
    reference=[rust_round(math.sin(math.tau*frequency*frame/48000)*12000) for frame in range(15360) for frequency in [440.,880.]]
    pcm=struct.pack('<'+'h'*len(reference),*reference)
    if digest(pcm)!=native['source_pcm_sha256']:raise ValueError('source reconstruction differs')
    (out/'input.s16le.gz').write_bytes(gzip.compress(pcm,mtime=0))
    lib=C.CDLL(str(library));lib.opus_get_version_string.restype=C.c_char_p
    lib.opus_decoder_create.argtypes=[C.c_int32,C.c_int,C.POINTER(C.c_int)];lib.opus_decoder_create.restype=C.c_void_p
    lib.opus_decoder_destroy.argtypes=[C.c_void_p];lib.opus_decoder_destroy.restype=None
    lib.opus_decode_float.argtypes=[C.c_void_p,C.POINTER(C.c_ubyte),C.c_int32,C.POINTER(C.c_float),C.c_int,C.c_int];lib.opus_decode_float.restype=C.c_int
    result={'schema':'gliss-audio-duration-retrospective-quality-v1','scope':__doc__.strip(),'source_pcm_sha256':digest(pcm),'native_original_sha256':digest(gzip.decompress(native_packed)),
      'script_sha256':digest(Path(__file__).read_bytes()),'plan_sha256':digest((HERE/'quality-plan.json').read_bytes()),'decoder_library_sha256':digest(library.read_bytes()),'decoder_version':lib.opus_get_version_string().decode('ascii'),
      'arms':[{'arm':i,'packet_frames':f,'status':'blocked'} for i,f in enumerate([960,1920,1920,960])],'quality_acceptability_proved':False}
    def persist():
        with (out/'quality.json').open('w') as f:json.dump(result,f,indent=2);f.write('\n');f.flush();__import__('os').fsync(f.fileno())
    persist()
    for row,arm in zip(result['arms'],native['arms']):
        row['status']='running';persist();decoder=None
        try:
            frames=arm['packet_frames'];assert frames==row['packet_frames'] and arm['source_frames']==15360 and arm['lookahead_frames']==312
            error=C.c_int();decoder=lib.opus_decoder_create(48000,2,C.byref(error))
            if error.value or not decoder:raise ValueError('decoder creation')
            samples=[]
            for index,packet in enumerate(arm['chunks']):
                payload=check_wire(packet,frames,index);data=(C.c_ubyte*len(payload)).from_buffer_copy(payload);block=(C.c_float*(frames*2))()
                if lib.opus_decode_float(decoder,data,len(payload),block,frames,0)!=frames:raise ValueError('decoder frame count')
                samples.extend(block)
            aligned=samples[312*2:(312+15360)*2]
            if len(aligned)!=len(reference) or not all(math.isfinite(x) for x in aligned):raise ValueError('aligned samples')
            raw=struct.pack('<'+'f'*len(aligned),*aligned);file=f'arm-{row["arm"]}.f32le.gz';(out/file).write_bytes(gzip.compress(raw,mtime=0))
            row.update(status='decoded',lookahead_frames=312,frames=15360,decoded_sha256=digest(raw),decoded_file=file,
              channels=[metrics(reference[c::2],aligned[c::2]) for c in range(2)])
        except Exception as error:row.update(status='failed',failure=type(error).__name__);persist();raise
        finally:
            if decoder:lib.opus_decoder_destroy(decoder)
        persist()
    print(json.dumps([{'frames':a['packet_frames'],'snr_db':[c['snr_db'] for c in a['channels']]} for a in result['arms']]))
if __name__=='__main__':main()
