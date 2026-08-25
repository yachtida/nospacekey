//! 表示属性プロバイダ（ITfDisplayAttributeProvider）。
//!
//! preedit / インライン予測へ付ける表示属性を 3 つ提供する:
//!   - `GUID_DISPLAY_ATTRIBUTE`        : solid 下線（入力中の既定）
//!   - `GUID_DISPLAY_ATTRIBUTE_TARGET` : 太下線（文節ナビゲーションの選択文節）
//!   - `GUID_DISPLAY_ATTRIBUTE_PREDICTION` : 灰色文字＋点線下線（予測ゴースト）
//!
//! TSF はアプリ側で属性 GUID → スタイルの対応を引くため、
//! プロバイダ・属性情報・列挙子の 3 役を実装する。

use std::cell::Cell;

use windows::core::{implement, Result, BOOL, BSTR, GUID};
use windows::Win32::Foundation::{E_INVALIDARG, S_FALSE};
use windows::Win32::Graphics::Gdi::COLOR_GRAYTEXT;
use windows::Win32::UI::TextServices::{
    IEnumTfDisplayAttributeInfo, IEnumTfDisplayAttributeInfo_Impl, ITfDisplayAttributeInfo,
    ITfDisplayAttributeInfo_Impl, ITfDisplayAttributeProvider_Impl, TF_ATTR_INPUT,
    TF_ATTR_TARGET_CONVERTED, TF_CT_SYSCOLOR, TF_DA_COLOR, TF_DA_COLOR_0, TF_DISPLAYATTRIBUTE,
    TF_LS_DOT, TF_LS_SOLID,
};

use crate::globals::{
    ComObjectGuard, GUID_DISPLAY_ATTRIBUTE, GUID_DISPLAY_ATTRIBUTE_PREDICTION,
    GUID_DISPLAY_ATTRIBUTE_TARGET,
};

#[derive(Clone, Copy)]
enum DisplayAttributeKind {
    Input,
    Target,
    PredictionGhost,
}

fn ghost_color() -> TF_DA_COLOR {
    TF_DA_COLOR {
        r#type: TF_CT_SYSCOLOR,
        Anonymous: TF_DA_COLOR_0 {
            nIndex: COLOR_GRAYTEXT.0,
        },
    }
}

fn display_attribute(kind: DisplayAttributeKind) -> TF_DISPLAYATTRIBUTE {
    match kind {
        DisplayAttributeKind::Input | DisplayAttributeKind::Target => TF_DISPLAYATTRIBUTE {
            crText: TF_DA_COLOR::default(),
            crBk: TF_DA_COLOR::default(),
            lsStyle: TF_LS_SOLID,
            fBoldLine: BOOL::from(matches!(kind, DisplayAttributeKind::Target)),
            crLine: TF_DA_COLOR::default(),
            bAttr: if matches!(kind, DisplayAttributeKind::Target) {
                TF_ATTR_TARGET_CONVERTED
            } else {
                TF_ATTR_INPUT
            },
        },
        DisplayAttributeKind::PredictionGhost => TF_DISPLAYATTRIBUTE {
            crText: ghost_color(),
            crBk: TF_DA_COLOR::default(),
            lsStyle: TF_LS_DOT,
            fBoldLine: BOOL(0),
            crLine: ghost_color(),
            bAttr: TF_ATTR_INPUT,
        },
    }
}

/// 表示属性情報。`target=false` は「solid 下線・入力中」（従来）、`target=true` は
/// 「太下線・変換対象」（文節ナビゲーションの選択文節）。
#[implement(ITfDisplayAttributeInfo)]
pub struct UnderlineInfo {
    kind: DisplayAttributeKind,
    // C-1: DLL_REF で生存数を数える（ホストが保持中の DLL アンロードによる UAF を防ぐ）。
    _guard: ComObjectGuard,
}

impl UnderlineInfo {
    pub fn new() -> Self {
        Self {
            kind: DisplayAttributeKind::Input,
            _guard: ComObjectGuard::new(),
        }
    }

    pub fn new_target() -> Self {
        Self {
            kind: DisplayAttributeKind::Target,
            _guard: ComObjectGuard::new(),
        }
    }

    pub fn new_prediction() -> Self {
        Self {
            kind: DisplayAttributeKind::PredictionGhost,
            _guard: ComObjectGuard::new(),
        }
    }
}

impl ITfDisplayAttributeInfo_Impl for UnderlineInfo_Impl {
    fn GetGUID(&self) -> Result<GUID> {
        Ok(match self.kind {
            DisplayAttributeKind::Input => GUID_DISPLAY_ATTRIBUTE,
            DisplayAttributeKind::Target => GUID_DISPLAY_ATTRIBUTE_TARGET,
            DisplayAttributeKind::PredictionGhost => GUID_DISPLAY_ATTRIBUTE_PREDICTION,
        })
    }

    fn GetDescription(&self) -> Result<BSTR> {
        Ok(BSTR::from(match self.kind {
            DisplayAttributeKind::Input => "nospacekey input",
            DisplayAttributeKind::Target => "nospacekey target clause",
            DisplayAttributeKind::PredictionGhost => "nospacekey inline prediction",
        }))
    }

    fn GetAttributeInfo(&self, pda: *mut TF_DISPLAYATTRIBUTE) -> Result<()> {
        let da = display_attribute(self.kind);
        unsafe {
            if !pda.is_null() {
                *pda = da;
            }
        }
        Ok(())
    }

    fn SetAttributeInfo(&self, _pda: *const TF_DISPLAYATTRIBUTE) -> Result<()> {
        // 動的変更は受け付けない。
        Ok(())
    }

    fn Reset(&self) -> Result<()> {
        Ok(())
    }
}

/// 属性情報の総数（既定下線＋選択文節の太下線）。
const ATTR_COUNT: u32 = 3;

fn attr_at(index: u32) -> ITfDisplayAttributeInfo {
    match index {
        0 => UnderlineInfo::new().into(),
        1 => UnderlineInfo::new_target().into(),
        _ => UnderlineInfo::new_prediction().into(),
    }
}

/// 表示属性 2 件を順に返す列挙子。
#[implement(IEnumTfDisplayAttributeInfo)]
pub struct AttrEnum {
    index: Cell<u32>,
    // C-1: DLL_REF で生存数を数える。
    _guard: ComObjectGuard,
}

impl AttrEnum {
    pub fn new() -> Self {
        Self {
            index: Cell::new(0),
            _guard: ComObjectGuard::new(),
        }
    }
}

impl IEnumTfDisplayAttributeInfo_Impl for AttrEnum_Impl {
    fn Clone(&self) -> Result<IEnumTfDisplayAttributeInfo> {
        // 位置も含めて複製する。
        let dup = AttrEnum {
            index: Cell::new(self.index.get()),
            _guard: ComObjectGuard::new(),
        };
        Ok(dup.into())
    }

    fn Next(
        &self,
        ulcount: u32,
        rginfo: *mut Option<ITfDisplayAttributeInfo>,
        pcfetched: *mut u32,
    ) -> Result<()> {
        let mut fetched: u32 = 0;
        if !rginfo.is_null() {
            while fetched < ulcount && self.index.get() < ATTR_COUNT {
                unsafe {
                    // 代入（`*p = v`）は旧値の drop_in_place を先に走らせる — ホストが未初期化の
                    // まま渡す配列では不定ポインタへの Release（UB）。write は上書きのみ。
                    rginfo
                        .add(fetched as usize)
                        .write(Some(attr_at(self.index.get())));
                }
                self.index.set(self.index.get() + 1);
                fetched += 1;
            }
        }
        unsafe {
            if !pcfetched.is_null() {
                *pcfetched = fetched;
            }
        }
        // 要求数に満たなければ S_FALSE。
        if fetched == ulcount {
            Ok(())
        } else {
            Err(S_FALSE.into())
        }
    }

    fn Reset(&self) -> Result<()> {
        self.index.set(0);
        Ok(())
    }

    fn Skip(&self, ulcount: u32) -> Result<()> {
        self.index
            .set((self.index.get().saturating_add(ulcount)).min(ATTR_COUNT));
        Ok(())
    }
}

// ---- プロバイダ本体は TextService に実装する ----
// `#[implement]` が生成する `TextService_Impl` に対し、別モジュールから trait を実装する。
// （windows-rs 0.62 では `_Impl` 型は公開され、クレート内の別モジュールから impl 可能。）
impl ITfDisplayAttributeProvider_Impl for crate::text_service::TextService_Impl {
    fn EnumDisplayAttributeInfo(&self) -> Result<IEnumTfDisplayAttributeInfo> {
        Ok(AttrEnum::new().into())
    }

    fn GetDisplayAttributeInfo(&self, guid: *const GUID) -> Result<ITfDisplayAttributeInfo> {
        unsafe {
            if guid.is_null() {
                return Err(E_INVALIDARG.into());
            }
            if *guid == GUID_DISPLAY_ATTRIBUTE {
                Ok(UnderlineInfo::new().into())
            } else if *guid == GUID_DISPLAY_ATTRIBUTE_TARGET {
                Ok(UnderlineInfo::new_target().into())
            } else if *guid == GUID_DISPLAY_ATTRIBUTE_PREDICTION {
                Ok(UnderlineInfo::new_prediction().into())
            } else {
                Err(E_INVALIDARG.into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Gdi::COLOR_GRAYTEXT;
    use windows::Win32::UI::TextServices::{TF_CT_SYSCOLOR, TF_LS_DOT};

    #[test]
    fn prediction_ghost_uses_gray_text_and_dotted_underline() {
        let attribute = display_attribute(DisplayAttributeKind::PredictionGhost);
        assert_eq!(attribute.crText.r#type, TF_CT_SYSCOLOR);
        assert_eq!(
            unsafe { attribute.crText.Anonymous.nIndex },
            COLOR_GRAYTEXT.0
        );
        assert_eq!(attribute.lsStyle, TF_LS_DOT);
        assert!(!attribute.fBoldLine.as_bool());
    }
}
