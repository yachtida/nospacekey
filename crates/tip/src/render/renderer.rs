//! DComp 上の premultiplied-alpha swapchain に Direct2D で描く SurfaceRenderer。
//!
//! IME は STA。COM ポインタは生成スレッド上でのみ使う前提（Send/Sync は付けない）。初期化は
//! 失敗しうる（GPU 無効/Win10/リモートデスクトップ等）ので `new` は Result を返し、呼び出し側が
//! GDI へフォールバックする。TIP パスなので内部で panic は絶対にしない（unwrap/expect 禁止）。
//!
//! 構成: D3D11(HARDWARE→WARP フォールバック) → DXGI → D2D1Factory1/Device/DeviceContext →
//! CreateSwapChainForComposition(premultiplied,B8G8R8A8) → DComp device/target/visual に載せる。
//! 各フレーム: begin_draw で BeginDraw、呼び出し側が描画、end_draw で EndDraw+Present+Commit。

use windows::core::{Interface, Result};
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct2D::Common::{D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_PIXEL_FORMAT};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1DeviceContext, ID2D1Factory1, D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
    D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES1, D2D1_DEVICE_CONTEXT_OPTIONS_NONE,
    D2D1_FACTORY_TYPE_SINGLE_THREADED,
};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
    IDCompositionVisual3,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIFactory2, IDXGISurface, IDXGISwapChain1, DXGI_PRESENT, DXGI_SCALING_STRETCH,
    DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
};

/// DComp+D2D の描画面。COM ポインタ一式を所有し、Drop で COM が解放される。
/// target/visual は直接触らないが、DComp ツリーから外れると描画が消えるので Drop まで保持する。
pub struct SurfaceRenderer {
    context: ID2D1DeviceContext,
    swapchain: IDXGISwapChain1,
    dcomp_device: IDCompositionDevice,
    // target/visual は Drop まで生かしておく必要があるので保持する（未使用でも drop 順のため）。
    _target: IDCompositionTarget,
    _visual: IDCompositionVisual,
    /// 出現/退場フェード用の Opacity 付き visual インタフェース（同じ visual への QI）。
    /// Win8.1 未満相当の環境で QI に失敗したら None（フェードなしで劣化）。
    visual3: Option<IDCompositionVisual3>,
    width: u32,
    height: u32,
}

impl SurfaceRenderer {
    pub fn new(hwnd: HWND, width: u32, height: u32) -> Result<SurfaceRenderer> {
        unsafe {
            // 1) D3D11 device（HARDWARE→WARP フォールバック）。BGRA_SUPPORT は D2D 相互運用に必須。
            //    HARDWARE が使えない環境（RDP/GPU 無効）でも WARP で描けるよう二段構えにする。
            let mut d3d: Option<ID3D11Device> = None;
            let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;
            let mut hr = D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                flags,
                None,
                D3D11_SDK_VERSION,
                Some(&mut d3d),
                None,
                None,
            );
            if hr.is_err() {
                hr = D3D11CreateDevice(
                    None,
                    D3D_DRIVER_TYPE_WARP,
                    HMODULE::default(),
                    flags,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut d3d),
                    None,
                    None,
                );
            }
            hr?;
            // out パラメータは Option。両ドライバ失敗の理論上のケースに備え unwrap せず Err にする。
            // hr は成功なのに device が None という矛盾ケースなので、直近のスレッドエラーで包む。
            let d3d = d3d.ok_or_else(windows::core::Error::from_thread)?;
            let dxgi_device: IDXGIDevice = d3d.cast()?;

            // 2) D2D factory/device/context。IME は STA なので SINGLE_THREADED で十分。
            let factory: ID2D1Factory1 =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let d2d_device = factory.CreateDevice(&dxgi_device)?;
            let context = d2d_device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;

            // 3) premultiplied composition swapchain。半透明の角丸/影を DComp 合成に載せるため
            //    premultiplied-alpha + B8G8R8A8 を選ぶ。composition 用なので幅高は必須（0 不可）。
            let adapter = dxgi_device.GetAdapter()?;
            let dxgi_factory: IDXGIFactory2 = adapter.GetParent()?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width.max(1),
                Height: height.max(1),
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
                ..Default::default()
            };
            // pdevice は IUnknown 派生を要求。restrict-to-output は使わないので None。
            let swapchain =
                dxgi_factory.CreateSwapChainForComposition(&dxgi_device, &desc, None)?;

            // 4) DComp device/target/visual に swapchain を載せる。topmost=true で候補窓を最前面へ。
            let dcomp_device: IDCompositionDevice = DCompositionCreateDevice(&dxgi_device)?;
            let target = dcomp_device.CreateTargetForHwnd(hwnd, true)?;
            let visual = dcomp_device.CreateVisual()?;
            visual.SetContent(&swapchain)?;
            target.SetRoot(&visual)?;
            dcomp_device.Commit()?;

            // Opacity は IDCompositionVisual3 のプロパティ。QI 失敗はフェードなしで劣化。
            let visual3: Option<IDCompositionVisual3> = visual.cast().ok();

            let r = SurfaceRenderer {
                context,
                swapchain,
                dcomp_device,
                _target: target,
                _visual: visual,
                visual3,
                width,
                height,
            };
            r.bind_backbuffer()?;
            Ok(r)
        }
    }

    /// swapchain の backbuffer を D2D の描画ターゲットにバインドする（生成/resize 後に呼ぶ）。
    /// CANNOT_DRAW は「ターゲット専用（他描画のソースにしない）」の宣言で、GPU に最適化余地を与える。
    unsafe fn bind_backbuffer(&self) -> Result<()> {
        let surface: IDXGISurface = self.swapchain.GetBuffer(0)?;
        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
            ..Default::default()
        };
        let bitmap = self
            .context
            .CreateBitmapFromDxgiSurface(&surface, Some(&props))?;
        self.context.SetTarget(&bitmap);
        Ok(())
    }

    pub fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        unsafe {
            // 既存ターゲットを外してから backbuffer を作り直す。SetTarget(None) しないと
            // ResizeBuffers が「バッファがまだ参照されている」で失敗する。
            self.context.SetTarget(None);
            self.swapchain.ResizeBuffers(
                0,
                width.max(1),
                height.max(1),
                DXGI_FORMAT_B8G8R8A8_UNORM,
                DXGI_SWAP_CHAIN_FLAG(0),
            )?;
            self.width = width;
            self.height = height;
            self.bind_backbuffer()
        }
    }

    /// 生の D2D context を直接触りたい呼び出し元向けの公開アクセサ。現状は begin_draw 経由の
    /// context しか使っていないため未参照だが、将来 B/C 段（設定アプリのプレビュー等）で
    /// begin_draw を介さない直接描画が要る場合に備えて残す（pub API として維持）。
    #[allow(dead_code)]
    pub fn context(&self) -> &ID2D1DeviceContext {
        &self.context
    }

    /// フレーム開始。BeginDraw を呼び、context を返す。BeginDraw 自体は失敗を返さない。
    pub fn begin_draw(&self) -> Result<&ID2D1DeviceContext> {
        unsafe {
            self.context.BeginDraw();
        }
        Ok(&self.context)
    }

    /// 出現/退場フェードが使えるか（visual の IDCompositionVisual3 QI に成功しているか）。
    /// 呼び出し側はこれが false のときフェード＋遅延 hide をせず即時 hide に劣化する
    /// （フェードだけ効かず hide が遅れる、という中途半端を避ける）。
    pub fn supports_opacity(&self) -> bool {
        self.visual3.is_some()
    }

    /// visual の不透明度を静的値に設定して Commit する。visual3 が無ければ no-op で Ok。
    /// フェード対象外の表示（reduced-motion 時や再表示の割り込み）で 1.0 へ戻すのに使う。
    pub fn set_opacity(&self, value: f32) -> Result<()> {
        let Some(v3) = self.visual3.as_ref() else {
            return Ok(());
        };
        unsafe {
            // windows-rs の命名: SetOpacity(アニメーション) / SetOpacity2(静的 f32)。
            v3.SetOpacity2(value)?;
            self.dcomp_device.Commit()?;
        }
        Ok(())
    }

    /// 不透明度を `from`→`to` へ `duration_ms` かけてイーズアウトで動かす（DComp が
    /// コンポジタ側で駆動するのでタイマ・打鍵経路の負荷はゼロ）。visual3 が無ければ no-op。
    ///
    /// 曲線は減衰比 1.0 相当のイーズアウト多項式 f(t) = from + k·(2(t/d) − (t/d)²)
    /// （k=to−from、終端で値 to・速度 0。オーバーシュートなし＝GUI の --ease-snap 対応）。
    pub fn animate_opacity(&self, from: f32, to: f32, duration_ms: f64) -> Result<()> {
        let Some(v3) = self.visual3.as_ref() else {
            return Ok(());
        };
        let d = (duration_ms / 1000.0).max(0.001);
        let k = to - from;
        unsafe {
            let anim = self.dcomp_device.CreateAnimation()?;
            // f(t) = from + (2k/d)·t + (−k/d²)·t²（3 次係数 0）。
            anim.AddCubic(
                0.0,
                from,
                (2.0 * k as f64 / d) as f32,
                (-(k as f64) / (d * d)) as f32,
                0.0,
            )?;
            anim.End(d, to)?;
            v3.SetOpacity(&anim)?;
            self.dcomp_device.Commit()?;
        }
        Ok(())
    }

    /// フレーム終了。EndDraw→Present→DComp Commit。Present は HRESULT なので .ok()? で伝播。
    /// syncinterval=1 で vsync 同期し、tearing を避ける。
    pub fn end_draw(&self) -> Result<()> {
        unsafe {
            self.context.EndDraw(None, None)?;
            self.swapchain.Present(1, DXGI_PRESENT(0)).ok()?;
            self.dcomp_device.Commit()?;
        }
        Ok(())
    }
}

/// end_draw 等の失敗がデバイスロスト（GPU リセット/TDR/RDP 遷移/スリープ復帰）由来かを判定する。
///
/// なぜ必要か: デバイスロストが起きると、以後 EndDraw/Present は毎フレーム同じ HRESULT を返し
/// 続けるため、レンダラを作り直さない限り窓が永久にブランクになる。呼び出し側はこの判定が真の
/// ときだけ「窓を破棄して次回 show で D2D レンダラを再生成」する（正常なフレーム失敗を巻き込んで
/// 窓を作り直さないよう、ロスト HRESULT に限定する）。
///
/// 判定する HRESULT（windows-0.62.2 実測・定数の所在も確認済み）:
/// - DXGI_ERROR_DEVICE_REMOVED  0x887A0005（Win32::Graphics::Dxgi）
/// - DXGI_ERROR_DEVICE_RESET    0x887A0007（Win32::Graphics::Dxgi）
/// - D2DERR_RECREATE_TARGET     0x8899000C（Win32::Foundation。D2D が「ターゲット作り直し」を要求）
pub fn is_device_lost(err: &windows::core::Error) -> bool {
    use windows::Win32::Foundation::D2DERR_RECREATE_TARGET;
    use windows::Win32::Graphics::Dxgi::{DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET};
    let code = err.code();
    code == DXGI_ERROR_DEVICE_REMOVED
        || code == DXGI_ERROR_DEVICE_RESET
        || code == D2DERR_RECREATE_TARGET
}

#[cfg(test)]
mod tests {
    use super::is_device_lost;
    use windows::core::{Error, HRESULT};

    #[test]
    fn is_device_lost_matches_known_hresults() {
        // デバイスロスト由来の 3 つの HRESULT は true。
        assert!(is_device_lost(&Error::from_hresult(HRESULT(0x887A0005_u32 as i32)))); // DEVICE_REMOVED
        assert!(is_device_lost(&Error::from_hresult(HRESULT(0x887A0007_u32 as i32)))); // DEVICE_RESET
        assert!(is_device_lost(&Error::from_hresult(HRESULT(0x8899000C_u32 as i32)))); // RECREATE_TARGET
    }

    #[test]
    fn is_device_lost_rejects_other_errors() {
        // 通常のフレーム失敗（例: E_FAIL / E_INVALIDARG）はロスト扱いにしない
        // （窓の作り直しを巻き込まないため）。
        assert!(!is_device_lost(&Error::from_hresult(HRESULT(0x80004005_u32 as i32)))); // E_FAIL
        assert!(!is_device_lost(&Error::from_hresult(HRESULT(0x80070057_u32 as i32)))); // E_INVALIDARG
        assert!(!is_device_lost(&Error::from_hresult(HRESULT(0)))); // S_OK
    }
}
