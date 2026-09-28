#pragma once
#include <stdint.h>
#ifdef __cplusplus
extern "C" {
#endif
typedef void (*nsk_windows_text_sink)(void *context, const uint16_t *text, uint32_t length);
// Returns an HRESULT. The sink is called synchronously and never retained.
int32_t nsk_windows_text_candidates(const uint16_t *reading, uint32_t length,
    int32_t prediction, uint32_t limit, uint32_t timeout_ms,
    nsk_windows_text_sink sink, void *context);
#ifdef __cplusplus
}
#endif
