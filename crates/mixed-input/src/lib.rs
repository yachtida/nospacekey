//! 日本語・英字混在入力の契約型と fixture 基盤。
//!
//! 設計の正本は `docs/adr/0007-mixed-input-segment-contract.md` と
//! `docs/active/2026-09-24-mixed-input-plan.md`。判別器本体（PR4）も TIP 側の
//! ワーカー・変換経路（PR5 以降）も、この crate の契約型を共有する。
//!
//! この crate は TSF・IPC・Swift・Zenzai に依存しない。契約型・fixture・検証・
//! 再生 CLI に加え、PR2 から元入力の由来（`source`）、Plan 適用の対応
//! （`projection`）、LocalKanaComposer と共有するローマ字表と読み再合成（`roman`）、
//! PR4 から区間判別（`classify`。n-gram・辞書・区間DP・学習・評価）を持つ。
//! TIP 側の表示・モデル呼出はここに含めない。

pub mod classify;
pub mod episode;
pub mod plan;
pub mod position;
pub mod projection;
pub mod replay;
pub mod roman;
pub mod source;
pub mod validation;

pub mod selection;

pub mod live;

pub mod quality;

pub mod assets;
