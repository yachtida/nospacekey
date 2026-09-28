#include "NospacekeyWindowsText.h"
#ifdef _WIN32
#define NOMINMAX
#include <windows.h>
#include <roapi.h>
#include <winrt/Windows.Foundation.h>
#include <winrt/Windows.Foundation.Collections.h>
#include <winrt/Windows.Data.Text.h>
#include <chrono>
#include <thread>
#include <vector>

extern "C" int32_t nsk_windows_text_candidates(const uint16_t *reading, uint32_t length,
    int32_t prediction, uint32_t limit, uint32_t timeout_ms,
    nsk_windows_text_sink sink, void *context) {
    if (!reading || !length || length > 4096 || !sink || !limit || limit > 256 ||
        !timeout_ms || timeout_ms > 3000) return E_INVALIDARG;
    const HRESULT initialized = RoInitialize(RO_INIT_MULTITHREADED);
    if (FAILED(initialized) && initialized != RPC_E_CHANGED_MODE) return initialized;
    struct Apartment {
        bool owned;
        ~Apartment() { if (owned) RoUninitialize(); }
    } apartment{SUCCEEDED(initialized)};
    try {
        using namespace winrt::Windows::Data::Text;
        using namespace winrt::Windows::Foundation;
        const auto started = std::chrono::steady_clock::now();
        const winrt::hstring input(reinterpret_cast<const wchar_t *>(reading), length);
        IAsyncOperation<Collections::IVectorView<winrt::hstring>> operation{nullptr};
        if (prediction) {
            TextPredictionGenerator generator{L"ja-JP"};
            if (generator.LanguageAvailableButNotInstalled()) return HRESULT_FROM_WIN32(ERROR_NOT_SUPPORTED);
            operation = generator.GetCandidatesAsync(input, limit);
        } else {
            TextConversionGenerator generator{L"ja-JP"};
            if (generator.LanguageAvailableButNotInstalled()) return HRESULT_FROM_WIN32(ERROR_NOT_SUPPORTED);
            operation = generator.GetCandidatesAsync(input, limit);
        }
        // This runs on the engine's conversion thread, without a window, key input,
        // or TSF activation. Do not use .get(): callers may already own an STA.
        while (operation.Status() == AsyncStatus::Started) {
            if (std::chrono::steady_clock::now() - started >= std::chrono::milliseconds(timeout_ms)) {
                operation.Cancel();
                return HRESULT_FROM_WIN32(ERROR_TIMEOUT);
            }
            std::this_thread::sleep_for(std::chrono::milliseconds(1));
        }
        const auto results = operation.GetResults();
        std::vector<winrt::hstring> seen;
        for (const auto &text : results) {
            if (text.empty() || text.size() > 4096) continue;
            bool duplicate = false;
            for (const auto &previous : seen) if (text == previous) { duplicate = true; break; }
            if (duplicate) continue;
            seen.push_back(text);
            sink(context, reinterpret_cast<const uint16_t *>(text.c_str()), text.size());
            if (seen.size() >= limit) break;
        }
        return S_OK;
    } catch (const winrt::hresult_error &error) { return error.code(); }
      catch (...) { return E_FAIL; }
}
#else
extern "C" int32_t nsk_windows_text_candidates(const uint16_t *, uint32_t,
    int32_t, uint32_t, uint32_t, nsk_windows_text_sink, void *) {
    return static_cast<int32_t>(0x80004001u);
}
#endif
