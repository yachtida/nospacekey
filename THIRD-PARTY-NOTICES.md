# Third-Party Notices

nospacekey is a from-scratch Windows TSF implementation deriving its
conversion engine from the azooKey input method project. The nospacekey
project itself is released under the MIT License (see the `LICENSE` file).

This distribution bundles and/or statically links a number of third-party
open-source components. Each component below is listed with a description of
what it is, its license, its copyright, and the full text of its license.

The license texts reproduced in full in this document:

- The **MIT License** text is included, in full, with each MIT-licensed
  component (the bodies are identical; only the copyright line differs). For
  the Rust crates table it is included by reference to those texts, applying
  with each crate's copyright holders as listed.
- The **Apache License, Version 2.0** text is long and standardized. Its full
  text is reproduced **once** in the "Apache License 2.0 — Full Text" section
  near the end of this file. Components licensed under Apache-2.0 reference
  that section rather than repeating the full text.
- **BSD-3-Clause** and the **Unicode License v3** are reproduced in the "Rust
  crates" section, together with an **MPL-2.0** source-availability notice for
  the single MPL-licensed crate.

A trailing **NOTE** section covers the Zenzai neural model, which is **not**
bundled with this distribution but which a user may optionally download.

---

## llama.cpp

**What it is:** LLM inference library. This distribution bundles the prebuilt
runtime libraries `llama.dll`, `ggml.dll`, `ggml-base.dll`, and `ggml-cpu.dll`,
which the engine host loads at runtime to perform Zenzai neural conversion on
the CPU. Built from the azooKey/llama.cpp fork, tag `b4846`.

**License:** MIT

**Copyright:** Copyright (c) 2023-2024 The ggml authors

```
MIT License

Copyright (c) 2023-2024 The ggml authors

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

---

## AzooKeyKanaKanjiConverter

**What it is:** The kana-kanji conversion engine. It is statically linked into
`NospacekeyEngineHost.exe` and provides the core Japanese conversion functionality
of this IME.

**License:** MIT

**Copyright:** Copyright (c) 2023 Miwa / Ensan

```
MIT License

Copyright (c) 2023 Miwa / Ensan

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

---

## Jinja (johnmai-dev/Jinja)

**What it is:** A Swift implementation of the Jinja templating engine, used to
render chat/prompt templates for the neural model. Statically linked into
`NospacekeyEngineHost.exe`.

**License:** MIT

**Copyright:** Copyright (c) 2024 John Mai

```
MIT License

Copyright (c) 2024 John Mai

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

---

## azooKey_dictionary_storage (default LOUDS dictionary)

**What it is:** The default azooKey Japanese dictionary data (LOUDS-encoded),
bundled inside the
`AzooKeyKanaKanjiConverter_KanaKanjiConverterModuleWithDefaultDictionary.resources`
directory of this distribution. This is the dictionary the converter consults
to produce conversion candidates.

**License:** Apache License, Version 2.0 (full text below)

**Copyright:** Copyright 2024 Miwa / ensan

---

## swift-collections (apple/swift-collections)

**What it is:** Apple's package of additional Swift data structures. Statically
linked into `NospacekeyEngineHost.exe`.

**License:** Apache License, Version 2.0 (full text below)

**Copyright:** Copyright 2020-2024 Apple Inc. and the Swift project authors

---

## swift-algorithms (apple/swift-algorithms)

**What it is:** Apple's package of sequence and collection algorithms.
Statically linked into `NospacekeyEngineHost.exe`.

**License:** Apache License, Version 2.0 (full text below)

**Copyright:** Copyright 2020-2024 Apple Inc. and the Swift project authors

---

## swift-numerics (apple/swift-numerics)

**What it is:** Apple's package of numerical types and functions. Statically
linked into `NospacekeyEngineHost.exe`.

**License:** Apache License, Version 2.0 (full text below)

**Copyright:** Copyright 2019-2024 Apple Inc. and the Swift project authors

---

## swift-tokenizers / swift-transformers (ensan-hcl/swift-tokenizers)

**What it is:** A Swift implementation of Hugging Face tokenizers (the
`swift-transformers` package, ensan-hcl fork), used to tokenize text for the
neural model. Statically linked into `NospacekeyEngineHost.exe`.

**License:** Apache License, Version 2.0 (full text below)

**Copyright:** Copyright 2022 Hugging Face SAS

---

## Swift runtime libraries (Apple / the Swift project)

**What it is:** The Swift toolchain runtime DLLs redistributed next to the engine
so it runs on a machine without the Swift toolchain installed — e.g.
`swiftCore.dll`, `Foundation.dll`, `FoundationEssentials.dll`,
`FoundationInternationalization.dll`, `FoundationNetworking.dll`,
`FoundationXML.dll`, `_FoundationICU.dll`, `dispatch.dll`, `BlocksRuntime.dll`,
`swiftDispatch.dll`, `swift_Concurrency.dll`, `swift_StringProcessing.dll`,
`swift_RegexParser.dll`, `swiftCRT.dll`, `swiftWinSDK.dll` and related libraries
(from the Swift 6.3.2 Windows runtime).

**License:** Apache License, Version 2.0, **with the Runtime Library Exception**
(full Apache text below; the Runtime Library Exception permits redistribution of
the runtime binaries without the exception's own attribution becoming viral).

**Copyright:** Copyright © Apple Inc. and the Swift project authors.

---

## Microsoft Visual C++ runtime

**What it is:** The Microsoft Visual C++ runtime DLLs that the MSVC-built binaries
statically import, redistributed app-locally so the package runs on a clean PC
without the VC++ Redistributable installed — `vcruntime140.dll`,
`vcruntime140_1.dll`, `msvcp140.dll`, and `vcomp140.dll` (the OpenMP runtime that
`ggml-cpu.dll` requires).

**License:** Redistributed under the Microsoft Visual Studio / Visual C++
Redistributable license terms, which permit app-local redistribution of these
runtime files. See the Visual Studio redistributable license for full terms.

**Copyright:** © Microsoft Corporation.

---

## Rust crates (statically linked into `nospacekey_tip.dll` and `NospacekeyConfig.exe`)

**What they are:** The Rust binaries in this distribution — the TSF text
service `nospacekey_tip.dll` and the settings GUI `NospacekeyConfig.exe` (Tauri) —
statically link the following third-party crates from crates.io. The list is
the union of the two binaries' *runtime* dependency graphs, resolved for the
`x86_64-pc-windows-msvc` target. Build-time-only tools and procedural macros
(e.g. `serde_derive`, `tauri-build`, `winresource`) are excluded because their
code is not contained in the distributed binaries.

Regenerate with:

```
cargo tree --target x86_64-pc-windows-msvc -e normal,no-proc-macro \
  -p nospacekey_tip -p config --prefix none | sort -u
```

**License elections for dual/multi-licensed crates:** where a crate is offered
under `MIT OR Apache-2.0` (or similar disjunctive terms including MIT), this
project elects the **MIT License**. For `dunce` (`CC0-1.0 OR MIT-0 OR
Apache-2.0`) it elects **Apache-2.0**. For crates offered under `Unlicense OR
MIT` it elects **MIT**. Conjunctive terms (`AND`) apply in full.

The license texts referenced below are reproduced elsewhere in this document:
MIT per-component above (the body is standard; it applies with each crate's
copyright holders listed in this table), Apache-2.0 in the "Apache License
2.0 — Full Text" section, and BSD-3-Clause / Unicode License v3 / the MPL-2.0
notice in the subsections immediately after this table.

| Crate | Version | License | Copyright / Authors |
|---|---|---|---|
| `aho-corasick` | 1.1.4 | Unlicense OR MIT | Andrew Gallant |
| `alloc-no-stdlib` | 2.0.4 | BSD-3-Clause | Daniel Reiter Horn |
| `alloc-stdlib` | 0.2.4 | BSD-3-Clause | Daniel Reiter Horn |
| `anyhow` | 1.0.103 | MIT OR Apache-2.0 | David Tolnay |
| `base64` | 0.22.1 | MIT OR Apache-2.0 | Marshall Pierce |
| `bitflags` | 2.13.0 | MIT OR Apache-2.0 | The Rust Project Developers |
| `brotli` | 8.0.4 | BSD-3-Clause AND MIT | Daniel Reiter Horn; The Brotli Authors |
| `brotli-decompressor` | 5.0.3 | BSD-3-Clause/MIT | Daniel Reiter Horn; The Brotli Authors |
| `byteorder` | 1.5.0 | Unlicense OR MIT | Andrew Gallant |
| `bytes` | 1.12.0 | MIT | Carl Lerche; Sean McArthur |
| `cfb` | 0.7.3 | MIT | Matthew D. Steele |
| `cfg-if` | 1.0.4 | MIT OR Apache-2.0 | Alex Crichton |
| `cookie` | 0.18.1 | MIT OR Apache-2.0 | Sergio Benitez; Alex Crichton |
| `crossbeam-channel` | 0.5.15 | MIT OR Apache-2.0 | the crossbeam-channel developers |
| `crossbeam-utils` | 0.8.21 | MIT OR Apache-2.0 | the crossbeam-utils developers |
| `ctor` | 0.8.0 | Apache-2.0 OR MIT | Matt Mastracci |
| `deranged` | 0.5.8 | MIT OR Apache-2.0 | Jacob Pratt |
| `dirs` | 6.0.0 | MIT OR Apache-2.0 | Simon Ochsenreither |
| `dirs-sys` | 0.5.0 | MIT OR Apache-2.0 | Simon Ochsenreither |
| `dpi` | 0.1.2 | Apache-2.0 AND MIT | the dpi developers |
| `dunce` | 1.0.5 | CC0-1.0 OR MIT-0 OR Apache-2.0 | Kornel |
| `equivalent` | 1.0.2 | Apache-2.0 OR MIT | the equivalent developers |
| `erased-serde` | 0.4.10 | MIT OR Apache-2.0 | David Tolnay |
| `fnv` | 1.0.7 | Apache-2.0 / MIT | Alex Crichton |
| `form_urlencoded` | 1.2.2 | MIT OR Apache-2.0 | The rust-url developers |
| `getrandom` | 0.3.4 | MIT OR Apache-2.0 | The Rand Project Developers |
| `glob` | 0.3.3 | MIT OR Apache-2.0 | The Rust Project Developers |
| `hashbrown` | 0.17.1 | MIT OR Apache-2.0 | the hashbrown developers |
| `heck` | 0.5.0 | MIT OR Apache-2.0 | the heck developers |
| `http` | 1.4.2 | MIT OR Apache-2.0 | Alex Crichton; Carl Lerche; Sean McArthur |
| `icu_collections` | 2.2.0 | Unicode-3.0 | The ICU4X Project Developers |
| `icu_locale_core` | 2.2.0 | Unicode-3.0 | The ICU4X Project Developers |
| `icu_normalizer` | 2.2.0 | Unicode-3.0 | The ICU4X Project Developers |
| `icu_normalizer_data` | 2.2.0 | Unicode-3.0 | The ICU4X Project Developers |
| `icu_properties` | 2.2.0 | Unicode-3.0 | The ICU4X Project Developers |
| `icu_properties_data` | 2.2.0 | Unicode-3.0 | The ICU4X Project Developers |
| `icu_provider` | 2.2.0 | Unicode-3.0 | The ICU4X Project Developers |
| `idna` | 1.1.0 | MIT OR Apache-2.0 | The rust-url developers |
| `idna_adapter` | 1.2.2 | Apache-2.0 OR MIT | The rust-url developers |
| `indexmap` | 2.14.0 | Apache-2.0 OR MIT | the indexmap developers |
| `infer` | 0.19.0 | MIT | Bojan |
| `itoa` | 1.0.18 | MIT OR Apache-2.0 | David Tolnay |
| `json-patch` | 3.0.1 | MIT/Apache-2.0 | Ivan Dubrov |
| `jsonptr` | 0.6.3 | MIT OR Apache-2.0 | chance dinkins; André Sá de Mello |
| `keyboard-types` | 0.7.0 | MIT OR Apache-2.0 | Pyfisch |
| `libc` | 0.2.186 | MIT OR Apache-2.0 | The Rust Project Developers |
| `litemap` | 0.8.2 | Unicode-3.0 | The ICU4X Project Developers |
| `lock_api` | 0.4.14 | MIT OR Apache-2.0 | Amanieu d'Antras |
| `log` | 0.4.33 | MIT OR Apache-2.0 | The Rust Project Developers |
| `memchr` | 2.8.2 | Unlicense OR MIT | Andrew Gallant; bluss |
| `mime` | 0.3.17 | MIT OR Apache-2.0 | Sean McArthur |
| `muda` | 0.19.3 | Apache-2.0 OR MIT | the muda developers |
| `num-conv` | 0.2.2 | MIT OR Apache-2.0 | Jacob Pratt |
| `once_cell` | 1.21.4 | MIT OR Apache-2.0 | Aleksey Kladov |
| `option-ext` | 0.2.0 | MPL-2.0 | Simon Ochsenreither |
| `parking_lot` | 0.12.5 | MIT OR Apache-2.0 | Amanieu d'Antras |
| `parking_lot_core` | 0.9.12 | MIT OR Apache-2.0 | Amanieu d'Antras |
| `percent-encoding` | 2.3.2 | MIT OR Apache-2.0 | The rust-url developers |
| `phf` | 0.13.1 | MIT | Steven Fackler |
| `phf_shared` | 0.13.1 | MIT | Steven Fackler |
| `pin-project-lite` | 0.2.17 | Apache-2.0 OR MIT | the pin-project-lite developers |
| `plist` | 1.9.0 | MIT | Ed Barnard |
| `potential_utf` | 0.1.5 | Unicode-3.0 | The ICU4X Project Developers |
| `powerfmt` | 0.2.0 | MIT OR Apache-2.0 | Jacob Pratt |
| `quick-xml` | 0.39.4 | MIT | the quick-xml developers |
| `raw-window-handle` | 0.6.2 | MIT OR Apache-2.0 OR Zlib | Osspial |
| `regex` | 1.12.4 | MIT OR Apache-2.0 | The Rust Project Developers; Andrew Gallant |
| `regex-automata` | 0.4.14 | MIT OR Apache-2.0 | The Rust Project Developers; Andrew Gallant |
| `regex-syntax` | 0.8.11 | MIT OR Apache-2.0 | The Rust Project Developers; Andrew Gallant |
| `rfd` | 0.16.0 | MIT | Poly |
| `same-file` | 1.0.6 | Unlicense/MIT | Andrew Gallant |
| `scopeguard` | 1.2.0 | MIT OR Apache-2.0 | bluss |
| `semver` | 1.0.28 | MIT OR Apache-2.0 | David Tolnay |
| `serde` | 1.0.228 | MIT OR Apache-2.0 | Erick Tryzelaar; David Tolnay |
| `serde-untagged` | 0.1.9 | MIT OR Apache-2.0 | David Tolnay |
| `serde_core` | 1.0.228 | MIT OR Apache-2.0 | Erick Tryzelaar; David Tolnay |
| `serde_json` | 1.0.150 | MIT OR Apache-2.0 | Erick Tryzelaar; David Tolnay |
| `serde_spanned` | 1.1.1 | MIT OR Apache-2.0 | the serde_spanned developers |
| `serde_with` | 3.21.0 | MIT OR Apache-2.0 | Jonas Bushart; Marcin Kaźmierczak |
| `serialize-to-javascript` | 0.1.2 | MIT OR Apache-2.0 | Chip Reed |
| `siphasher` | 1.0.3 | MIT/Apache-2.0 | Frank Denis |
| `smallvec` | 1.15.2 | MIT OR Apache-2.0 | The Servo Project Developers |
| `softbuffer` | 0.4.8 | MIT OR Apache-2.0 | the softbuffer developers |
| `stable_deref_trait` | 1.2.1 | MIT OR Apache-2.0 | Robert Grosse |
| `tao` | 0.35.3 | Apache-2.0 | Tauri Programme within The Commons Conservancy; The winit contributors |
| `tauri` | 2.11.5 | Apache-2.0 OR MIT | Tauri Programme within The Commons Conservancy |
| `tauri-plugin-dialog` | 2.7.1 | Apache-2.0 OR MIT | Tauri Programme within The Commons Conservancy |
| `tauri-plugin-fs` | 2.5.1 | Apache-2.0 OR MIT | Tauri Programme within The Commons Conservancy |
| `tauri-runtime` | 2.11.3 | Apache-2.0 OR MIT | Tauri Programme within The Commons Conservancy |
| `tauri-runtime-wry` | 2.11.4 | Apache-2.0 OR MIT | Tauri Programme within The Commons Conservancy |
| `tauri-utils` | 2.9.3 | Apache-2.0 OR MIT | Tauri Programme within The Commons Conservancy |
| `thiserror` | 1.0.69 | MIT OR Apache-2.0 | David Tolnay |
| `thiserror` | 2.0.18 | MIT OR Apache-2.0 | David Tolnay |
| `time` | 0.3.53 | MIT OR Apache-2.0 | Jacob Pratt; Time contributors |
| `time-core` | 0.1.9 | MIT OR Apache-2.0 | Jacob Pratt; Time contributors |
| `tinystr` | 0.8.3 | Unicode-3.0 | The ICU4X Project Developers |
| `tokio` | 1.52.3 | MIT | Tokio Contributors |
| `toml` | 1.1.2+spec-1.1.0 | MIT OR Apache-2.0 | the toml developers |
| `toml_datetime` | 1.1.1+spec-1.1.0 | MIT OR Apache-2.0 | the toml_datetime developers |
| `toml_parser` | 1.1.2+spec-1.1.0 | MIT OR Apache-2.0 | the toml_parser developers |
| `toml_writer` | 1.1.1+spec-1.1.0 | MIT OR Apache-2.0 | the toml_writer developers |
| `tracing` | 0.1.44 | MIT | Eliza Weisman; Tokio Contributors |
| `tracing-core` | 0.1.36 | MIT | Tokio Contributors |
| `typeid` | 1.0.3 | MIT OR Apache-2.0 | David Tolnay |
| `unic-char-property` | 0.9.0 | MIT/Apache-2.0 | The UNIC Project Developers |
| `unic-char-range` | 0.9.0 | MIT/Apache-2.0 | The UNIC Project Developers |
| `unic-common` | 0.9.0 | MIT/Apache-2.0 | The UNIC Project Developers |
| `unic-ucd-ident` | 0.9.0 | MIT/Apache-2.0 | The UNIC Project Developers |
| `unic-ucd-version` | 0.9.0 | MIT/Apache-2.0 | The UNIC Project Developers |
| `unicode-segmentation` | 1.13.3 | MIT OR Apache-2.0 | kwantam; Manish Goregaokar |
| `url` | 2.5.8 | MIT OR Apache-2.0 | The rust-url developers |
| `urlpattern` | 0.3.0 | MIT | the Deno authors; crowlKats |
| `utf8_iter` | 1.0.4 | Apache-2.0 OR MIT | Henri Sivonen |
| `uuid` | 1.23.4 | Apache-2.0 OR MIT | Ashley Mannix; Dylan DPC; Hunar Roop Kahlon |
| `walkdir` | 2.5.0 | Unlicense/MIT | Andrew Gallant |
| `webview2-com` | 0.38.2 | MIT | the webview2-com developers |
| `webview2-com-sys` | 0.38.2 | MIT | the webview2-com-sys developers |
| `winapi-util` | 0.1.11 | Unlicense OR MIT | Andrew Gallant |
| `window-vibrancy` | 0.6.0 | Apache-2.0 OR MIT | Tauri Programme within The Commons Conservancy |
| `windows` | 0.61.3 | MIT OR Apache-2.0 | Microsoft |
| `windows` | 0.62.2 | MIT OR Apache-2.0 | the windows developers |
| `windows-collections` | 0.2.0 | MIT OR Apache-2.0 | the windows-collections developers |
| `windows-collections` | 0.3.2 | MIT OR Apache-2.0 | the windows-collections developers |
| `windows-core` | 0.61.2 | MIT OR Apache-2.0 | Microsoft |
| `windows-core` | 0.62.2 | MIT OR Apache-2.0 | the windows-core developers |
| `windows-future` | 0.2.1 | MIT OR Apache-2.0 | the windows-future developers |
| `windows-future` | 0.3.2 | MIT OR Apache-2.0 | the windows-future developers |
| `windows-link` | 0.1.3 | MIT OR Apache-2.0 | Microsoft |
| `windows-link` | 0.2.1 | MIT OR Apache-2.0 | the windows-link developers |
| `windows-numerics` | 0.2.0 | MIT OR Apache-2.0 | the windows-numerics developers |
| `windows-numerics` | 0.3.1 | MIT OR Apache-2.0 | the windows-numerics developers |
| `windows-registry` | 0.6.1 | MIT OR Apache-2.0 | the windows-registry developers |
| `windows-result` | 0.3.4 | MIT OR Apache-2.0 | Microsoft |
| `windows-result` | 0.4.1 | MIT OR Apache-2.0 | the windows-result developers |
| `windows-strings` | 0.4.2 | MIT OR Apache-2.0 | Microsoft |
| `windows-strings` | 0.5.1 | MIT OR Apache-2.0 | the windows-strings developers |
| `windows-sys` | 0.59.0 | MIT OR Apache-2.0 | Microsoft |
| `windows-sys` | 0.60.2 | MIT OR Apache-2.0 | Microsoft |
| `windows-sys` | 0.61.2 | MIT OR Apache-2.0 | the windows-sys developers |
| `windows-targets` | 0.52.6 | MIT OR Apache-2.0 | Microsoft |
| `windows-targets` | 0.53.5 | MIT OR Apache-2.0 | the windows-targets developers |
| `windows-threading` | 0.1.0 | MIT OR Apache-2.0 | Microsoft |
| `windows-threading` | 0.2.1 | MIT OR Apache-2.0 | the windows-threading developers |
| `windows-version` | 0.1.7 | MIT OR Apache-2.0 | the windows-version developers |
| `windows_x86_64_msvc` | 0.52.6 | MIT OR Apache-2.0 | Microsoft |
| `windows_x86_64_msvc` | 0.53.1 | MIT OR Apache-2.0 | the windows_x86_64_msvc developers |
| `winnow` | 1.0.3 | MIT | the winnow developers |
| `writeable` | 0.6.3 | Unicode-3.0 | The ICU4X Project Developers |
| `wry` | 0.55.1 | Apache-2.0 OR MIT | Tauri Programme within The Commons Conservancy |
| `yoke` | 0.8.3 | Unicode-3.0 | Manish Goregaokar |
| `zerofrom` | 0.1.8 | Unicode-3.0 | The ICU4X Project Developers |
| `zeroize` | 1.9.0 | Apache-2.0 OR MIT | The RustCrypto Project Developers |
| `zerotrie` | 0.2.4 | Unicode-3.0 | The ICU4X Project Developers |
| `zerovec` | 0.11.6 | Unicode-3.0 | The ICU4X Project Developers |
| `zmij` | 1.0.21 | MIT | David Tolnay |

Total: 155 crates.

### BSD-3-Clause (alloc-no-stdlib, alloc-stdlib, brotli, brotli-decompressor)

Copyright (c) the copyright holders listed in the table above (Daniel Reiter
Horn / Dropbox, Inc., and The Brotli Authors).

```
Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice,
   this list of conditions and the following disclaimer.

2. Redistributions in binary form must reproduce the above copyright notice,
   this list of conditions and the following disclaimer in the documentation
   and/or other materials provided with the distribution.

3. Neither the name of the copyright holder nor the names of its contributors
   may be used to endorse or promote products derived from this software
   without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

### Unicode License v3 (ICU4X crates)

Applies to the `icu_*`, `litemap`, `potential_utf`, `tinystr`, `writeable`,
`yoke`, `zerofrom`, `zerotrie`, `zerovec` crates marked `Unicode-3.0` in the
table above. Copyright © Unicode, Inc.

```
UNICODE LICENSE V3

COPYRIGHT AND PERMISSION NOTICE

Copyright © 1991-2023 Unicode, Inc.

NOTICE TO USER: Carefully read the following legal agreement. BY
DOWNLOADING, INSTALLING, COPYING OR OTHERWISE USING DATA FILES, AND/OR
SOFTWARE, YOU UNEQUIVOCALLY ACCEPT, AND AGREE TO BE BOUND BY, ALL OF THE
TERMS AND CONDITIONS OF THIS AGREEMENT. IF YOU DO NOT AGREE, DO NOT
DOWNLOAD, INSTALL, COPY, DISTRIBUTE OR USE THE DATA FILES OR SOFTWARE.

Permission is hereby granted, free of charge, to any person obtaining a
copy of data files and any associated documentation (the "Data Files") or
software and any associated documentation (the "Software") to deal in the
Data Files or Software without restriction, including without limitation
the rights to use, copy, modify, merge, publish, distribute, and/or sell
copies of the Data Files or Software, and to permit persons to whom the
Data Files or Software are furnished to do so, provided that either (a)
this copyright and permission notice appear with all copies of the Data
Files or Software, or (b) this copyright and permission notice appear in
associated Documentation.

THE DATA FILES AND SOFTWARE ARE PROVIDED "AS IS", WITHOUT WARRANTY OF ANY
KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT OF
THIRD PARTY RIGHTS.

IN NO EVENT SHALL THE COPYRIGHT HOLDER OR HOLDERS INCLUDED IN THIS NOTICE
BE LIABLE FOR ANY CLAIM, OR ANY SPECIAL INDIRECT OR CONSEQUENTIAL DAMAGES,
OR ANY DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS,
WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION,
ARISING OUT OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THE DATA
FILES OR SOFTWARE.

Except as contained in this notice, the name of a copyright holder shall
not be used in advertising or otherwise to promote the sale, use or other
dealings in these Data Files or Software without prior written
authorization of the copyright holder.
```

### MPL-2.0 notice (option-ext)

The `option-ext` crate is licensed under the Mozilla Public License, Version
2.0 (https://mozilla.org/MPL/2.0/). It is statically linked, unmodified, into
`NospacekeyConfig.exe`. As required by MPL-2.0 §3.2, recipients are informed that
the complete, unmodified Source Code Form of `option-ext` is available at
https://github.com/soc/option-ext (version as listed in the table above, also
obtainable via `cargo download option-ext`). MPL-2.0 applies only to the files
of that crate; it does not extend to the rest of this distribution.

---

## Apache License 2.0 — Full Text

The following is the full text of the Apache License, Version 2.0, which
governs the components above that are marked "Apache License, Version 2.0
(full text below)".

```
                                 Apache License
                           Version 2.0, January 2004
                        http://www.apache.org/licenses/

   TERMS AND CONDITIONS FOR USE, REPRODUCTION, AND DISTRIBUTION

   1. Definitions.

      "License" shall mean the terms and conditions for use, reproduction,
      and distribution as defined by Sections 1 through 9 of this document.

      "Licensor" shall mean the copyright owner or entity authorized by
      the copyright owner that is granting the License.

      "Legal Entity" shall mean the union of the acting entity and all
      other entities that control, are controlled by, or are under common
      control with that entity. For the purposes of this definition,
      "control" means (i) the power, direct or indirect, to cause the
      direction or management of such entity, whether by contract or
      otherwise, or (ii) ownership of fifty percent (50%) or more of the
      outstanding shares, or (iii) beneficial ownership of such entity.

      "You" (or "Your") shall mean an individual or Legal Entity
      exercising permissions granted by this License.

      "Source" form shall mean the preferred form for making modifications,
      including but not limited to software source code, documentation
      source, and configuration files.

      "Object" form shall mean any form resulting from mechanical
      transformation or translation of a Source form, including but
      not limited to compiled object code, generated documentation,
      and conversions to other media types.

      "Work" shall mean the work of authorship, whether in Source or
      Object form, made available under the License, as indicated by a
      copyright notice that is included in or attached to the work
      (an example is provided in the Appendix below).

      "Derivative Works" shall mean any work, whether in Source or Object
      form, that is based on (or derived from) the Work and for which the
      editorial revisions, annotations, elaborations, or other modifications
      represent, as a whole, an original work of authorship. For the purposes
      of this License, Derivative Works shall not include works that remain
      separable from, or merely link (or bind by name) to the interfaces of,
      the Work and Derivative Works thereof.

      "Contribution" shall mean any work of authorship, including
      the original version of the Work and any modifications or additions
      to that Work or Derivative Works thereof, that is intentionally
      submitted to Licensor for inclusion in the Work by the copyright owner
      or by an individual or Legal Entity authorized to submit on behalf of
      the copyright owner. For the purposes of this definition, "submitted"
      means any form of electronic, verbal, or written communication sent
      to the Licensor or its representatives, including but not limited to
      communication on electronic mailing lists, source code control systems,
      and issue tracking systems that are managed by, or on behalf of, the
      Licensor for the purpose of discussing and improving the Work, but
      excluding communication that is conspicuously marked or otherwise
      designated in writing by the copyright owner as "Not a Contribution."

      "Contributor" shall mean Licensor and any individual or Legal Entity
      on behalf of whom a Contribution has been received by Licensor and
      subsequently incorporated within the Work.

   2. Grant of Copyright License. Subject to the terms and conditions of
      this License, each Contributor hereby grants to You a perpetual,
      worldwide, non-exclusive, no-charge, royalty-free, irrevocable
      copyright license to reproduce, prepare Derivative Works of,
      publicly display, publicly perform, sublicense, and distribute the
      Work and such Derivative Works in Source or Object form.

   3. Grant of Patent License. Subject to the terms and conditions of
      this License, each Contributor hereby grants to You a perpetual,
      worldwide, non-exclusive, no-charge, royalty-free, irrevocable
      (except as stated in this section) patent license to make, have made,
      use, offer to sell, sell, import, and otherwise transfer the Work,
      where such license applies only to those patent claims licensable
      by such Contributor that are necessarily infringed by their
      Contribution(s) alone or by combination of their Contribution(s)
      with the Work to which such Contribution(s) was submitted. If You
      institute patent litigation against any entity (including a
      cross-claim or counterclaim in a lawsuit) alleging that the Work
      or a Contribution incorporated within the Work constitutes direct
      or contributory patent infringement, then any patent licenses
      granted to You under this License for that Work shall terminate
      as of the date such litigation is filed.

   4. Redistribution. You may reproduce and distribute copies of the
      Work or Derivative Works thereof in any medium, with or without
      modifications, and in Source or Object form, provided that You
      meet the following conditions:

      (a) You must give any other recipients of the Work or
          Derivative Works a copy of this License; and

      (b) You must cause any modified files to carry prominent notices
          stating that You changed the files; and

      (c) You must retain, in the Source form of any Derivative Works
          that You distribute, all copyright, patent, trademark, and
          attribution notices from the Source form of the Work,
          excluding those notices that do not pertain to any part of
          the Derivative Works; and

      (d) If the Work includes a "NOTICE" text file as part of its
          distribution, then any Derivative Works that You distribute must
          include a readable copy of the attribution notices contained
          within such NOTICE file, excluding those notices that do not
          pertain to any part of the Derivative Works, in at least one
          of the following places: within a NOTICE text file distributed
          as part of the Derivative Works; within the Source form or
          documentation, if provided along with the Derivative Works; or,
          within a display generated by the Derivative Works, if and
          wherever such third-party notices normally appear. The contents
          of the NOTICE file are for informational purposes only and
          do not modify the License. You may add Your own attribution
          notices within Derivative Works that You distribute, alongside
          or as an addendum to the NOTICE text from the Work, provided
          that such additional attribution notices cannot be construed
          as modifying the License.

      You may add Your own copyright statement to Your modifications and
      may provide additional or different license terms and conditions
      for use, reproduction, or distribution of Your modifications, or
      for any such Derivative Works as a whole, provided Your use,
      reproduction, and distribution of the Work otherwise complies with
      the conditions stated in this License.

   5. Submission of Contributions. Unless You explicitly state otherwise,
      any Contribution intentionally submitted for inclusion in the Work
      by You to the Licensor shall be under the terms and conditions of
      this License, without any additional terms or conditions.
      Notwithstanding the above, nothing herein shall supersede or modify
      the terms of any separate license agreement you may have executed
      with Licensor regarding such Contributions.

   6. Trademarks. This License does not grant permission to use the trade
      names, trademarks, service marks, or product names of the Licensor,
      except as required for reasonable and customary use in describing the
      origin of the Work and reproducing the content of the NOTICE file.

   7. Disclaimer of Warranty. Unless required by applicable law or
      agreed to in writing, Licensor provides the Work (and each
      Contributor provides its Contributions) on an "AS IS" BASIS,
      WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or
      implied, including, without limitation, any warranties or conditions
      of TITLE, NON-INFRINGEMENT, MERCHANTABILITY, or FITNESS FOR A
      PARTICULAR PURPOSE. You are solely responsible for determining the
      appropriateness of using or redistributing the Work and assume any
      risks associated with Your exercise of permissions under this License.

   8. Limitation of Liability. In no event and under no legal theory,
      whether in tort (including negligence), contract, or otherwise,
      unless required by applicable law (such as deliberate and grossly
      negligent acts) or agreed to in writing, shall any Contributor be
      liable to You for damages, including any direct, indirect, special,
      incidental, or consequential damages of any character arising as a
      result of this License or out of the use or inability to use the
      Work (including but not limited to damages for loss of goodwill,
      work stoppage, computer failure or malfunction, or any and all
      other commercial damages or losses), even if such Contributor
      has been advised of the possibility of such damages.

   9. Accepting Warranty or Additional Liability. While redistributing
      the Work or Derivative Works thereof, You may choose to offer,
      and charge a fee for, acceptance of support, warranty, indemnity,
      or other liability obligations and/or rights consistent with this
      License. However, in accepting such obligations, You may act only
      on Your own behalf and on Your sole responsibility, not on behalf
      of any other Contributor, and only if You agree to indemnify,
      defend, and hold each Contributor harmless for any liability
      incurred by, or claims asserted against, such Contributor by reason
      of your accepting any such warranty or additional liability.

   END OF TERMS AND CONDITIONS

   APPENDIX: How to apply the Apache License to your work.

      To apply the Apache License to your work, attach the following
      boilerplate notice, with the fields enclosed by brackets "[]"
      replaced with your own identifying information. (Don't include
      the brackets!)  The text should be enclosed in the appropriate
      comment syntax for the file format. We also recommend that a
      file or class name and description of purpose be included on the
      same "printed page" as the copyright notice for easier
      identification within third-party archives.

   Copyright [yyyy] [name of copyright owner]

   Licensed under the Apache License, Version 2.0 (the "License");
   you may not use this file except in compliance with the License.
   You may obtain a copy of the License at

       http://www.apache.org/licenses/LICENSE-2.0

   Unless required by applicable law or agreed to in writing, software
   distributed under the License is distributed on an "AS IS" BASIS,
   WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
   See the License for the specific language governing permissions and
   limitations under the License.
```

---

## NOTE: Zenzai neural model (NOT included in this distribution)

The **Zenzai neural model** — `zenz-v3.1-small` (`ggml-model-Q5_K_M.gguf`) by
Miwa-Keita — is licensed under **CC-BY-SA-4.0**
(https://huggingface.co/Miwa-Keita/zenz-v3.1-small-gguf).

This model file is **NOT included in this distribution**. If you opt into
Zenzai neural conversion, you download the model yourself and place it in the
`models\` folder. The CC-BY-SA-4.0 terms apply to that model file. The
Creative Commons Attribution-ShareAlike 4.0 International license text is
available at https://creativecommons.org/licenses/by-sa/4.0/legalcode.

This is a usage note for an optional, user-supplied file and does not
constitute bundling or redistribution of the model by this project.
