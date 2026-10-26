# Third-party notices

Gliss's original code is licensed under the root [MIT license](LICENSE). That license does not replace the licenses of bundled code, linked libraries, optional gateway dependencies or external browsers and VM software. This file identifies the principal components and their source/license locations; a binary distribution must also retain the notices required by its resolved dependency graph.

## Bundled browser inflater

The browser includes a modified **fflate 0.8.3** inflater by **Arjun Barrett**, under MIT:

- [Upstream project and version](https://github.com/101arrowz/fflate/tree/v0.8.3).
- [Retained upstream copyright and MIT text](web/vendor/fflate-LICENSE), also preserved with the [upstream reconstruction inputs](third_party/fflate/vendor/LICENSE).
- [Production provenance](web/vendor/fflate-provenance.json), [patch recipe](third_party/fflate/fflate-patches.json) and [reconstruction guide](third_party/fflate/README.md).

`web/vendor/fflate-bounded-core.js` is a guarded fork, not unmodified upstream. Gliss adds fixed output storage, stream-extent/checksum checks and stricter malformed-input/history validation. Preserve the upstream notice with redistributed copies, including a browser asset bundle. The patch recipe and provenance distinguish Gliss's modifications from the original implementation.

## OpenH264 and Rust bindings

The [video crate](crates/gliss-video/Cargo.toml) pins **openh264 0.9.8** and selects its `source` feature with default features disabled. The [encoder](crates/gliss-video/src/lib.rs) uses `OpenH264API::from_source()`: this configuration compiles the bundled native source rather than selecting Cisco's separately supplied prebuilt library.

| Component | Attribution and license | Source |
| --- | --- | --- |
| `openh264` and `openh264-sys2` Rust bindings | Ralf Biedert; BSD-2-Clause, as declared by the resolved packages | [openh264-rs](https://github.com/ralfbiedert/openh264-rs), [versioned package documentation](https://docs.rs/openh264/0.9.8/openh264/) |
| Bundled OpenH264 native core | Copyright (c) 2013, Cisco Systems; BSD-2-Clause | [Cisco OpenH264](https://github.com/cisco/openh264), [license in the resolved source package](https://docs.rs/crate/openh264-sys2/0.9.8/source/upstream/LICENSE) |

Retain the bindings' license information and the native core notice when distributing a compiled Gliss server. Selecting Deflate at runtime does not remove the server's declared video dependency. Cisco's separately distributed binary has its own distribution terms; do not infer that those terms apply to this source-built configuration. Gliss's MIT license is not a statement about H.264 patent rights or royalties.

The native core's retained notice is:

```text
Copyright (c) 2013, Cisco Systems
All rights reserved.

Redistribution and use in source and binary forms, with or without modification,
are permitted provided that the following conditions are met:

* Redistributions of source code must retain the above copyright notice, this
  list of conditions and the following disclaimer.

* Redistributions in binary form must reproduce the above copyright notice, this
  list of conditions and the following disclaimer in the documentation and/or
  other materials provided with the distribution.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND
ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR
ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES
(INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES;
LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON
ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

The aarch64 build also compiles OpenH264's `codec/common/arm64/arm_aarch64_common.h`, which carries this notice:

```text
Copyright (c) 2024, ARM Ltd.
All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions
are met:

* Redistributions of source code must retain the above copyright
  notice, this list of conditions and the following disclaimer.

* Redistributions in binary form must reproduce the above copyright
  notice, this list of conditions and the following disclaimer in
  the documentation and/or other materials provided with the
  distribution.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
"AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS
FOR A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE
COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT,
INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING,
BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES;
LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT
LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN
ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE
POSSIBILITY OF SUCH DAMAGE.
```

## Resolved dependencies and external tools

[Cargo.lock](Cargo.lock) records the Rust dependency versions, and [licenses/rust-dependencies.md](licenses/rust-dependencies.md) collects the license texts of every crate linked into the released binary. Their package manifests and accompanying license/notice files remain authoritative for those packages and their native dependencies. This includes the HTTP/WebSocket, async, compression, cryptography, QUIC and D-Bus stacks; this document is not an exhaustive license-text bundle for every target or build feature. Preserve the resolved packages' required notices with redistributed binaries.

The optional Python gateway pins **aiohttp 3.13.5** in its [requirements file](integration/distribox/prototype/requirements.txt); see [aiohttp's source and license](https://github.com/aio-libs/aiohttp/tree/v3.13.5). Its installed dependencies retain their own licenses. They are not embedded into the Rust executable by that requirements file.

The browser test package declares **Playwright 1.61.1** as a development dependency. [package-lock.json](package-lock.json) records Apache-2.0 for Playwright and Playwright Core; see [the upstream project](https://github.com/microsoft/playwright). Chromium is installed separately and has its own notices and third-party components. A browser installation is not relicensed by the Gliss test package.

QEMU, libvirt, Guacamole/guacd and Distribox are external systems used for interoperability or historical comparisons, not software relicensed by this repository. Their licenses apply when distributing those systems or derived images.
