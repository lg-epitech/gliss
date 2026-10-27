"""Build a diskless test guest from this Linux host's kernel modules and ALSA.

Usage: python test-support/qemu-audio/build.py <temporary-output-directory>
The directory must be empty. Requires gcc, libasound development files, modprobe,
cpio and a readable host kernel matching the installed modules.
"""
import gzip
import lzma
import pathlib
import re
import shutil
import subprocess
import sys


def build(directory):
    root = directory / "root"
    root.mkdir()
    source = pathlib.Path(__file__).with_name("init.c")
    subprocess.run(["gcc", "-O2", "-o", str(root / "init"), str(source),
                    "-lasound", "-lm"], check=True)
    # Copy the dynamic loader and libraries at the paths encoded by the host.
    libraries = subprocess.check_output(["ldd", str(root / "init")], text=True)
    for name in re.findall(r"(/\S+)\s+\(", libraries):
        target = root / name.lstrip("/")
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(name, target)
    program_headers = subprocess.check_output(["readelf", "-l", str(root / "init")], text=True)
    interpreter = re.search(r"Requesting program interpreter: (\S+)\]", program_headers).group(1)
    target = root / interpreter.lstrip("/")
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(interpreter, target)
    (root / "drivers").mkdir()
    modules = []
    seen = set()
    for driver in ("snd-hda-codec-generic", "snd-hda-intel"):
        dependencies = subprocess.check_output(
            ["modprobe", "--show-depends", driver], text=True)
        for line in dependencies.splitlines():
            if not line.startswith("insmod "):
                continue
            source = pathlib.Path(line.split()[1])
            if source in seen:
                continue
            seen.add(source)
            suffix = source.suffix
            name = source.stem if suffix in (".gz", ".xz", ".zst") else source.name
            target = root / "drivers" / name
            if suffix == ".zst":
                data = subprocess.check_output(["zstd", "-dc", str(source)])
            elif suffix == ".xz":
                data = lzma.decompress(source.read_bytes())
            elif suffix == ".gz":
                data = gzip.decompress(source.read_bytes())
            else:
                data = source.read_bytes()
            target.write_bytes(data)
            modules.append("/drivers/" + name)
    (root / "modules").write_text("\n".join(modules) + "\n")
    (root / "alsa.conf").write_text(
        "pcm.test { type hw card 0 device 0 }\nctl.test { type hw card 0 }\n")
    listing = subprocess.check_output(["find", ".", "-print0"], cwd=root)
    initramfs = directory / "initramfs.cpio"
    with initramfs.open("wb") as output:
        subprocess.run(["cpio", "--null", "-o", "--format=newc"], cwd=root,
                       input=listing, stdout=output, check=True)
    return initramfs


if __name__ == "__main__":
    print(build(pathlib.Path(sys.argv[1]).resolve()))
