//! B段: タスクバー入力インジケータ（langbar `GUID_LBI_INPUTMODE`）に出す HICON を実行時生成
//! するための純粋ロジック（ラベル/サイズ/配色の決定）。GDI 依存の実際の描画は
//! `render_mode_icon`（本ファイル下部、build-only）が行う。

use windows::Win32::Foundation::COLORREF;

/// モードから langbar アイコンに描く文字を返す。`langbar::mode_label_ephemeral` と同一結果
/// （トレイも HUD/言語バーと同じマーカーで ephemeral かなを区別する）。
pub fn icon_label(is_direct: bool, ephemeral: bool) -> &'static str {
    crate::langbar::mode_label_ephemeral(is_direct, ephemeral)
}

/// 96 DPI 基準ピクセル数を実行時 DPI へ整数丸めでスケールする。
/// `candidate_window.rs` の DPI スケール規則（四捨五入相当）と揃える。
pub fn icon_size_px(base_at_96dpi: i32, dpi: i32) -> i32 {
    (base_at_96dpi * dpi + 48) / 96
}

/// タスクバーのライト/ダーク判定結果。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IconTheme {
    Light,
    Dark,
}

/// `HKCU\...\Personalize\SystemUsesLightTheme` の DWORD 値からタスクバーの明暗を判定する。
/// 値が読めない場合は Light を既定にする（黒文字アイコンの方が誤爆時に見えやすい）。
pub fn icon_theme_from_registry_value(system_uses_light_theme: Option<u32>) -> IconTheme {
    match system_uses_light_theme {
        Some(0) => IconTheme::Dark,
        _ => IconTheme::Light,
    }
}

/// テーマに応じたアイコン前景色（COLORREF は 0x00BBGGRR）。
pub fn icon_fg_color(theme: IconTheme) -> COLORREF {
    match theme {
        IconTheme::Light => COLORREF(0x0020_2020), // candidate_window.rs COLOR_TEXT と同値
        IconTheme::Dark => COLORREF(0x00F0_F0F0),
    }
}

/// タスクバー/システムの明暗設定を読む。キー/値が無い（Win10 一部ビルド等）場合は None。
/// なぜ Option か: レジストリキー自体が無い環境があり得るため、呼び出し側
/// （`icon_theme_from_registry_value`）で Light 既定にフォールバックする設計。
pub fn read_system_uses_light_theme() -> Option<u32> {
    let key = windows_registry::CURRENT_USER
        .open(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
        .ok()?;
    key.get_u32("SystemUsesLightTheme").ok()
}

/// size×size の premultiplied BGRA バッファからインク（非ゼロ画素）の外接矩形を求め、
/// 中央寄せに必要な平行移動 (dx, dy) を返す純関数。インクが無ければ (0,0)。
/// バッファの行方向（bottom-up/top-down）に依らず、中央寄せは対称なので同じ式でよい。
///
/// なぜこの関数が必要か: フォントのサイドベアリング差（字送り幅の中で黒画素がどこに
/// 収まるか）は DT_CENTER/DT_CALCRECT のどちらを使っても吸収できない（どちらも字送り
/// 幅ベースの計測・配置であり、実際のインク位置は見ていないため）。描画後の実ピクセルの
/// 外接矩形を測り、そこを基準に中央化することで初めてベアリング差を打ち消せる。
pub(crate) fn ink_centering_shift(buf: &[u32], size: i32) -> (i32, i32) {
    if size <= 0 {
        return (0, 0);
    }
    let size_u = size as usize;
    let mut min_x = i32::MAX;
    let mut max_x = i32::MIN;
    let mut min_y = i32::MAX;
    let mut max_y = i32::MIN;
    for y in 0..size_u {
        for x in 0..size_u {
            let idx = y * size_u + x;
            if idx >= buf.len() {
                continue;
            }
            if buf[idx] != 0 {
                let xi = x as i32;
                let yi = y as i32;
                if xi < min_x {
                    min_x = xi;
                }
                if xi > max_x {
                    max_x = xi;
                }
                if yi < min_y {
                    min_y = yi;
                }
                if yi > max_y {
                    max_y = yi;
                }
            }
        }
    }
    if min_x > max_x {
        // インク無し。
        return (0, 0);
    }
    let ink_w = max_x - min_x + 1;
    let ink_h = max_y - min_y + 1;
    let dx = (size - ink_w) / 2 - min_x;
    let dy = (size - ink_h) / 2 - min_y;
    (dx, dy)
}

/// `is_direct` から あ/A を実行時に 32bpp ARGB ビットマップへ描画し HICON にする。
/// GDI/OS 依存につきユニットテスト不可（Task 4 の実機確認で検証）。失敗時は None
/// （呼び出し側 GetIcon は E_NOTIMPL にフォールバックし、システムは既定ロゴを出す）。
///
/// SAFETY: 呼び出し側（GetIcon 経由、TSF から STA スレッドで呼ばれる）で GDI ハンドルの
/// ライフタイムを閉じたスコープ内に収める。生成した HICON の所有権は呼び出し元に渡る
/// （破棄は呼び出し側の責務。Task 3 でキャッシュ＋Drop 解放する）。
///
/// なぜ全早期リターンで解放するか: GDI オブジェクトはプロセスあたり上限があるため、
/// 1 回のリーク（例: langbar からの頻繁な GetIcon 呼び出し）でも積み重なると
/// タスクバー描画全体が壊れる。失敗時こそ確実に後始末する。
pub(crate) unsafe fn render_mode_icon(
    is_direct: bool,
    ephemeral: bool,
    dpi: i32,
) -> Option<windows::Win32::UI::WindowsAndMessaging::HICON> {
    use windows::core::BOOL;
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, DrawTextW, GetDC,
        ReleaseDC, SelectObject, SetBkMode, SetTextColor, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
        DIB_RGB_COLORS, DT_CALCRECT, DT_NOPREFIX, DT_SINGLELINE, TRANSPARENT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, ICONINFO};

    let size = icon_size_px(16, dpi);
    if size <= 0 {
        return None;
    }

    let hdc_screen = GetDC(None);
    let hdc = CreateCompatibleDC(Some(hdc_screen));
    let _ = ReleaseDC(None, hdc_screen);
    if hdc.is_invalid() {
        return None;
    }

    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: core::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size,
            biHeight: size, // 正: ボトムアップ DIB（下から上）。CreateIconIndirect はこれで問題ない。
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut core::ffi::c_void = core::ptr::null_mut();
    let color_bmp = match CreateDIBSection(Some(hdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
        Ok(h) if !bits.is_null() => h,
        _ => {
            let _ = DeleteDC(hdc);
            return None;
        }
    };
    // 背景を全透明(alpha=0)で初期化。
    let px_count = (size as usize) * (size as usize);
    core::ptr::write_bytes(bits as *mut u32, 0, px_count);

    let old_bmp = SelectObject(hdc, color_bmp.into());
    let theme = icon_theme_from_registry_value(read_system_uses_light_theme());
    let fg = icon_fg_color(theme);
    // 10.5pt。トレイアイコンは OS 側の面なので theme.font_family ではなく既定 UI フォント固定。
    let family = crate::popup::family_utf16z("Yu Gothic UI");
    let hfont = crate::popup::create_font(&family, 105, dpi);
    let old_font = SelectObject(hdc, hfont.into());
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, fg);

    let label = icon_label(is_direct, ephemeral);
    let mut wtext: Vec<u16> = label.encode_utf16().collect();
    wtext.push(0);
    // DT_CENTER は「字送り幅(advance width)」基準で中央寄せするため、あ/A のようにインク
    // （実際の黒画素範囲）が字送り幅内で左右非対称なグリフでは、見た目が左にズレる。
    // 先に DT_CALCRECT で実測してインク幅・高さを求め、(size-w)/2, (size-h)/2 に置いて描く
    // ことでインク基準の中央寄せにする。DT_CALCRECT は描画せず矩形の right/bottom を測るだけ
    // なので、実測時と描画時で同じ DC・同じフォントが選択されている必要がある（本関数では
    // 直前で hfont を SelectObject 済みのため満たされる）。
    let mut calc = RECT { left: 0, top: 0, right: size, bottom: size };
    DrawTextW(hdc, &mut wtext, &mut calc, DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX);
    let text_w = calc.right - calc.left;
    let text_h = calc.bottom - calc.top;
    let left = (size - text_w) / 2;
    let top = (size - text_h) / 2;
    let mut rect = RECT { left, top, right: left + text_w, bottom: top + text_h };
    DrawTextW(hdc, &mut wtext, &mut rect, DT_SINGLELINE | DT_NOPREFIX);

    SelectObject(hdc, old_font);
    if !hfont.is_invalid() {
        let _ = DeleteObject(hfont.into());
    }
    SelectObject(hdc, old_bmp);
    let _ = DeleteDC(hdc);

    // premultiplied alpha 化: DrawTextW は alpha を書かないため、非ゼロ色チャンネルを
    // 不透明(alpha=255)として扱う簡易処理（GDI ClearType のサブピクセル値は捨てる代わりに
    // アイコンサイズが小さくアンチエイリアスの破綻が目立ちにくいことを実機確認で許容する）。
    let buf = core::slice::from_raw_parts_mut(bits as *mut u32, px_count);
    for px in buf.iter_mut() {
        let b = (*px & 0xFF) as u8;
        let g = ((*px >> 8) & 0xFF) as u8;
        let r = ((*px >> 16) & 0xFF) as u8;
        if r != 0 || g != 0 || b != 0 {
            *px = 0xFF00_0000 | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
        }
    }

    // ピクセル単位のインク中央寄せ: DT_CALCRECT は字送り幅を測るだけで、あ/A のような
    // グリフの左右非対称なサイドベアリングまでは補正できないため、実際に描画された
    // 非ゼロ画素の外接矩形を基準に平行移動する（なぜ＝上の DrawTextW コメント参照）。
    let (dx, dy) = ink_centering_shift(buf, size);
    if dx != 0 || dy != 0 {
        let mut shifted = vec![0u32; px_count];
        for y in 0..size {
            for x in 0..size {
                let src_idx = (y as usize) * (size as usize) + (x as usize);
                if buf[src_idx] == 0 {
                    continue;
                }
                let nx = x + dx;
                let ny = y + dy;
                if nx < 0 || nx >= size || ny < 0 || ny >= size {
                    continue; // 範囲外は破棄（パニックさせない）。
                }
                let dst_idx = (ny as usize) * (size as usize) + (nx as usize);
                shifted[dst_idx] = buf[src_idx];
            }
        }
        buf.copy_from_slice(&shifted);
    }

    // マスクは 1bpp 全 0（AND マスクなし＝カラー画像の alpha を使う）。
    let mask_bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: core::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size,
            biHeight: size,
            biPlanes: 1,
            biBitCount: 1,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let hdc_screen2 = GetDC(None);
    let mut mask_bits: *mut core::ffi::c_void = core::ptr::null_mut();
    let mask_bmp = CreateDIBSection(Some(hdc_screen2), &mask_bmi, DIB_RGB_COLORS, &mut mask_bits, None, 0);
    let _ = ReleaseDC(None, hdc_screen2);
    let mask_bmp = match mask_bmp {
        Ok(h) if !mask_bits.is_null() => {
            let mask_bytes = ((size as usize + 31) / 32) * 4 * size as usize;
            core::ptr::write_bytes(mask_bits as *mut u8, 0, mask_bytes);
            h
        }
        _ => {
            let _ = DeleteObject(color_bmp.into());
            return None;
        }
    };

    let icon_info = ICONINFO {
        fIcon: BOOL(1),
        xHotspot: 0,
        yHotspot: 0,
        hbmMask: mask_bmp,
        hbmColor: color_bmp,
    };
    let result = CreateIconIndirect(&icon_info).ok();
    let _ = DeleteObject(color_bmp.into());
    let _ = DeleteObject(mask_bmp.into());
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_label_matches_mode() {
        assert_eq!(icon_label(true, false), "A");
        assert_eq!(icon_label(false, false), "あ");
    }

    #[test]
    fn icon_label_shows_ephemeral_marker() {
        assert_eq!(icon_label(false, true), "あ˙");
    }

    #[test]
    fn icon_size_scales_with_dpi() {
        // 96 DPI: そのまま。192 DPI: 倍。
        assert_eq!(icon_size_px(16, 96), 16);
        assert_eq!(icon_size_px(16, 192), 32);
        // 144 DPI (150%): 24px 相当。
        assert_eq!(icon_size_px(16, 144), 24);
    }

    #[test]
    fn theme_from_registry_value_light_and_dark() {
        assert_eq!(icon_theme_from_registry_value(Some(1)), IconTheme::Light);
        assert_eq!(icon_theme_from_registry_value(Some(0)), IconTheme::Dark);
    }

    #[test]
    fn theme_from_registry_value_missing_defaults_light() {
        assert_eq!(icon_theme_from_registry_value(None), IconTheme::Light);
    }

    #[test]
    fn fg_color_differs_by_theme() {
        assert_ne!(icon_fg_color(IconTheme::Light), icon_fg_color(IconTheme::Dark));
    }

    #[test]
    fn ink_centering_shift_empty_buffer_is_noop() {
        let buf = vec![0u32; 25]; // size=5, all zero (no ink)
        assert_eq!(ink_centering_shift(&buf, 5), (0, 0));
    }

    #[test]
    fn ink_centering_shift_single_pixel_at_origin() {
        let size = 5;
        let mut buf = vec![0u32; (size * size) as usize];
        buf[0] = 0xFFFF_FFFF; // (0,0)
        assert_eq!(ink_centering_shift(&buf, size), (2, 2));
    }

    #[test]
    fn ink_centering_shift_already_centered_block_is_noop() {
        let size = 5;
        let mut buf = vec![0u32; (size * size) as usize];
        // 3x3 block at min=(1,1) .. max=(3,3) is already centered in size=5.
        for y in 1..=3 {
            for x in 1..=3 {
                buf[(y * size + x) as usize] = 0xFFFF_FFFF;
            }
        }
        assert_eq!(ink_centering_shift(&buf, size), (0, 0));
    }

    #[test]
    fn ink_centering_shift_edge_pixel_yields_negative_dx() {
        let size = 5;
        let mut buf = vec![0u32; (size * size) as usize];
        buf[(2 * size + 4) as usize] = 0xFFFF_FFFF; // (4,2)
        assert_eq!(ink_centering_shift(&buf, size), (-2, 0));
    }
}
