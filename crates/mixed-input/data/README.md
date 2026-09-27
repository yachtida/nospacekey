# Mixed-input asset notice

The mixed-input classifier and dictionaries are first-party nospacekey assets,
Copyright (c) 2026 nospacekey project contributors, under the MIT License shipped
as LICENSE. This permits use, modification and redistribution, including commercial
use, subject to retaining the copyright and license notice. No external corpus or
dictionary is bundled for this classifier.

The char-ngram-linear model (format 1) was trained offline from the self-authored
synthetic templates in crates/mixed-input/src/classify/synth.rs with seed 42.
Dictionary entries in data/dictionary/general.txt and tech.txt are self-authored
lists of common spellings. Training uses template-separated train/validation/frozen
sets. PR7 adjusts thresholds on validation only. This is not evidence of natural
input quality: candidate and automatic public release gates remain closed.

mixed-input-manifest.toml records the generation, byte lengths and SHA-256 hashes
of the assets embedded in nospacekey_tip.dll. It is distribution audit metadata,
not a file read by the installed TIP. The DLL validates its embedded copy on the
background worker before using mixed input. Invalid assets disable only that path.
No runtime model downloads, external file loading, hot replacement or personal
classifier training are provided. OFF stops mixed requests; reopen the input app
to release the old DLL and model. Update/uninstall owns the DLL and its assets.
Clearing user learning does not delete this immutable classifier.

Developer regeneration: follow docs/validation/mixed-input/README.md to train and
calibrate, then update data/manifest.toml generation, bytes and SHA-256 for model.txt,
dictionary/general.txt and dictionary/tech.txt. Hash the exact UTF-8 LF file bytes:
Get-FileHash -Algorithm SHA256 <file>; (Get-Item <file>).Length.
The manifest is schema 1, kind char-ngram-linear, model_format 1. Bounds are 1 MiB
model, 64 KiB per dictionary, 4096 entries per layer, 64 scalars per word. The model
parser additionally bounds n-gram orders/counts and rejects non-finite weights.
Run cargo test -p mixed-input assets:: through scripts/with-dev-env.ps1 before
building/staging. A stale manifest fails validation; do not bypass the check.
