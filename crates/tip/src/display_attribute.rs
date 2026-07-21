//! 表示属性プロバイダ（ITfDisplayAttributeProvider）。
//!
//! preedit に「solid な下線」を付けるための表示属性を 1 つだけ提供する。
//! TSF はアプリ側で属性 GUID → スタイルの対応を引くため、
//! プロバイダ・属性情報・列挙子の 3 役を実装する。

use std::cell::Cell;

use windows::core::{implement, Result, BSTR, GUID, BOOL};
use windows::Win32::Foundation::{E_INVALIDARG, S_FALSE};
use windows::Win32::UI::TextServices::{
    IEnumTfDisplayAttributeInfo, IEnumTfDisplayAttributeInfo_Impl, ITfDisplayAttributeInfo,
    ITfDisplayAttributeInfo_Impl, ITfDisplayAttributeProvider_Impl, TF_ATTR_INPUT, TF_DA_COLOR,
    TF_DISPLAYATTRIBUTE, TF_LS_SOLID,
};

use crate::globals::{ComObjectGuard, GUID_DISPLAY_ATTRIBUTE};

/// 「solid 下線・入力中」を表す単一の表示属性情報。
#[implement(ITfDisplayAttributeInfo)]
pub struct UnderlineInfo {
    // C-1: DLL_REF で生存数を数える（ホストが保持中の DLL アンロードによる UAF を防ぐ）。
    _guard: ComObjectGuard,
}

impl UnderlineInfo {
    pub fn new() -> Self {
        Self { _guard: ComObjectGuard::new() }
    }
}

impl ITfDisplayAttributeInfo_Impl for UnderlineInfo_Impl {
    fn GetGUID(&self) -> Result<GUID> {
        Ok(GUID_DISPLAY_ATTRIBUTE)
    }

    fn GetDescription(&self) -> Result<BSTR> {
        Ok(BSTR::from("nospacekey input"))
    }

    fn GetAttributeInfo(&self, pda: *mut TF_DISPLAYATTRIBUTE) -> Result<()> {
        // solid な下線。文字色・背景色・下線色は既定（自動）にしておく。
        let da = TF_DISPLAYATTRIBUTE {
            crText: TF_DA_COLOR::default(),
            crBk: TF_DA_COLOR::default(),
            lsStyle: TF_LS_SOLID,
            fBoldLine: BOOL::from(false),
            crLine: TF_DA_COLOR::default(),
            bAttr: TF_ATTR_INPUT,
        };
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

/// `UnderlineInfo` を 1 回だけ返す列挙子。
#[implement(IEnumTfDisplayAttributeInfo)]
pub struct AttrEnum {
    done: Cell<bool>,
    // C-1: DLL_REF で生存数を数える。
    _guard: ComObjectGuard,
}

impl AttrEnum {
    pub fn new() -> Self {
        Self {
            done: Cell::new(false),
            _guard: ComObjectGuard::new(),
        }
    }
}

impl IEnumTfDisplayAttributeInfo_Impl for AttrEnum_Impl {
    fn Clone(&self) -> Result<IEnumTfDisplayAttributeInfo> {
        // 位置も含めて複製する。
        let dup = AttrEnum {
            done: Cell::new(self.done.get()),
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
        // 要素は 1 つだけ。未取得かつ要求数 >= 1 のときに 1 件返す。
        if ulcount >= 1 && !self.done.get() && !rginfo.is_null() {
            let info: ITfDisplayAttributeInfo = UnderlineInfo::new().into();
            unsafe {
                *rginfo = Some(info);
            }
            self.done.set(true);
            fetched = 1;
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
        self.done.set(false);
        Ok(())
    }

    fn Skip(&self, ulcount: u32) -> Result<()> {
        if ulcount >= 1 {
            self.done.set(true);
        }
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

    fn GetDisplayAttributeInfo(
        &self,
        guid: *const GUID,
    ) -> Result<ITfDisplayAttributeInfo> {
        unsafe {
            if !guid.is_null() && *guid == GUID_DISPLAY_ATTRIBUTE {
                Ok(UnderlineInfo::new().into())
            } else {
                Err(E_INVALIDARG.into())
            }
        }
    }
}
