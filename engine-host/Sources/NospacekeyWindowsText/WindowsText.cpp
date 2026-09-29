#include "NospacekeyWindowsText.h"
#ifdef _WIN32
#define NOMINMAX
#include <windows.h>
#include <roapi.h>
#include <winrt/Windows.Foundation.h>
#include <winrt/Windows.Foundation.Collections.h>
#include <winrt/Windows.Data.Text.h>
#include <chrono>
#include <condition_variable>
#include <deque>
#include <memory>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

namespace {
using Clock = std::chrono::steady_clock;
using namespace winrt::Windows::Data::Text;
using namespace winrt::Windows::Foundation;
constexpr HRESULT timeout = HRESULT_FROM_WIN32(ERROR_TIMEOUT);

struct Query {
    std::wstring reading;
    uint32_t limit;
    Clock::time_point deadline;
    std::mutex mutex;
    std::condition_variable done;
    bool completed = false;
    HRESULT status = E_FAIL;
    std::vector<std::wstring> values;
};

// Windows generators and their MTA stay alive on the same thread between requests.
// Separate lanes prevent a slow prediction from delaying explicit conversion.
// Jobs own their input/results; a timed-out caller never leaves a borrowed sink behind.
class Worker {
    const bool prediction;
    std::mutex mutex;
    std::condition_variable ready;
    std::deque<std::shared_ptr<Query>> pending;
    bool stopping = false;
    std::thread thread;

    void run() {
        const HRESULT initialized = RoInitialize(RO_INIT_MULTITHREADED);
        {
            TextPredictionGenerator predictor{nullptr};
            TextConversionGenerator converter{nullptr};
            for (;;) {
                std::shared_ptr<Query> job;
                {
                    std::unique_lock lock(mutex);
                    ready.wait(lock, [&] { return stopping || !pending.empty(); });
                    if (stopping) break;
                    job = std::move(pending.front());
                    pending.pop_front();
                }
                HRESULT status = initialized;
                std::vector<std::wstring> values;
                try {
                    if (FAILED(initialized)) winrt::throw_hresult(initialized);
                    if (Clock::now() >= job->deadline) winrt::throw_hresult(timeout);
                    IAsyncOperation<Collections::IVectorView<winrt::hstring>> operation{nullptr};
                    const winrt::hstring input(job->reading);
                    if (prediction) {
                        if (!predictor) predictor = TextPredictionGenerator{L"ja-JP"};
                        if (predictor.LanguageAvailableButNotInstalled())
                            winrt::throw_hresult(HRESULT_FROM_WIN32(ERROR_NOT_SUPPORTED));
                        operation = predictor.GetCandidatesAsync(input, job->limit);
                    } else {
                        if (!converter) converter = TextConversionGenerator{L"ja-JP"};
                        if (converter.LanguageAvailableButNotInstalled())
                            winrt::throw_hresult(HRESULT_FROM_WIN32(ERROR_NOT_SUPPORTED));
                        operation = converter.GetCandidatesAsync(input, job->limit);
                    }
                    const auto remaining = std::chrono::duration_cast<TimeSpan>(job->deadline - Clock::now());
                    // This thread owns an MTA. One bounded completion wait avoids
                    // adding the Windows scheduler's sleep interval to every query.
                    if (remaining <= TimeSpan::zero() || operation.wait_for(remaining) == AsyncStatus::Started) {
                        operation.Cancel();
                        winrt::throw_hresult(timeout);
                    }
                    for (const auto &text : operation.GetResults()) {
                        if (text.empty() || text.size() > 4096) continue;
                        std::wstring value(text.c_str(), text.size());
                        bool duplicate = false;
                        for (const auto &previous : values) if (value == previous) { duplicate = true; break; }
                        if (duplicate) continue;
                        values.push_back(std::move(value));
                        if (values.size() >= job->limit) break;
                    }
                    status = S_OK;
                } catch (const winrt::hresult_error &error) { status = error.code(); }
                  catch (...) { status = E_FAIL; }
                if (FAILED(status)) {
                    // Recreate a failed/cancelled generator on the next request.
                    predictor = nullptr;
                    converter = nullptr;
                }
                {
                    std::lock_guard lock(job->mutex);
                    job->status = status;
                    job->values = std::move(values);
                    job->completed = true;
                }
                job->done.notify_one();
            }
        }
        if (SUCCEEDED(initialized)) RoUninitialize();
    }
public:
    explicit Worker(bool prediction) : prediction(prediction), thread([this] { run(); }) {}
    ~Worker() {
        { std::lock_guard lock(mutex); stopping = true; }
        ready.notify_one();
        thread.join();
    }
    HRESULT query(const uint16_t *reading, uint32_t length, uint32_t limit,
                  Clock::time_point deadline, nsk_windows_text_sink sink, void *context) {
        auto job = std::make_shared<Query>();
        job->reading.assign(reinterpret_cast<const wchar_t *>(reading), length);
        job->limit = limit;
        job->deadline = deadline;
        {
            std::lock_guard lock(mutex);
            if (pending.size() >= 16) return HRESULT_FROM_WIN32(ERROR_BUSY);
            pending.push_back(job);
        }
        ready.notify_one();
        std::unique_lock lock(job->mutex);
        if (!job->done.wait_until(lock, deadline, [&] { return job->completed; })) return timeout;
        if (FAILED(job->status)) return job->status;
        for (const auto &value : job->values)
            sink(context, reinterpret_cast<const uint16_t *>(value.data()), static_cast<uint32_t>(value.size()));
        return S_OK;
    }
};
}

extern "C" int32_t nsk_windows_text_candidates(const uint16_t *reading, uint32_t length,
    int32_t prediction, uint32_t limit, uint32_t timeout_ms,
    nsk_windows_text_sink sink, void *context) {
    if (!reading || !length || length > 4096 || !sink || !limit || limit > 256 ||
        !timeout_ms || timeout_ms > 3000) return E_INVALIDARG;
    try {
        const auto deadline = Clock::now() + std::chrono::milliseconds(timeout_ms);
        if (prediction) {
            static Worker worker(true);
            return worker.query(reading, length, limit, deadline, sink, context);
        }
        static Worker worker(false);
        return worker.query(reading, length, limit, deadline, sink, context);
    } catch (const winrt::hresult_error &error) { return error.code(); }
      catch (...) { return E_FAIL; }
}
#else
extern "C" int32_t nsk_windows_text_candidates(const uint16_t *, uint32_t,
    int32_t, uint32_t, uint32_t, nsk_windows_text_sink, void *) {
    return static_cast<int32_t>(0x80004001u);
}
#endif
