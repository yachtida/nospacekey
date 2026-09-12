#ifndef NOSPACEKEY_LLAMA_RUNTIME_H
#define NOSPACEKEY_LLAMA_RUNTIME_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define NSK_LLAMA_RUNTIME_ABI_VERSION UINT32_C(1)

#define NSK_LLAMA_RUNTIME_OK INT32_C(0)
#define NSK_LLAMA_RUNTIME_ERROR INT32_C(-1)

#define NSK_LLAMA_RUNTIME_STATE_UNCONFIGURED UINT32_C(0)
#define NSK_LLAMA_RUNTIME_STATE_GPU_ACTIVE UINT32_C(1)
#define NSK_LLAMA_RUNTIME_STATE_FAILED UINT32_C(2)

#define NSK_LLAMA_RUNTIME_FAILURE_NONE UINT32_C(0)
#define NSK_LLAMA_RUNTIME_FAILURE_INVALID_RUNTIME_DIRECTORY UINT32_C(1)
#define NSK_LLAMA_RUNTIME_FAILURE_BACKEND_PATH_REJECTED UINT32_C(2)
#define NSK_LLAMA_RUNTIME_FAILURE_BACKEND_UNAVAILABLE UINT32_C(3)
#define NSK_LLAMA_RUNTIME_FAILURE_GPU_UNAVAILABLE UINT32_C(4)
#define NSK_LLAMA_RUNTIME_FAILURE_MODEL_LOAD UINT32_C(5)
#define NSK_LLAMA_RUNTIME_FAILURE_CONTEXT_LOAD UINT32_C(6)
#define NSK_LLAMA_RUNTIME_FAILURE_DECODE UINT32_C(7)

#define NSK_LLAMA_RUNTIME_BACKEND_CAPACITY UINT32_C(32)
#define NSK_LLAMA_RUNTIME_DEVICE_CAPACITY UINT32_C(128)

/* Fixed-width fields keep this result stable across the Swift/C++ boundary. */
struct nsk_llama_runtime_status {
    uint32_t abi_version;
    uint32_t struct_size;
    uint32_t state;
    uint32_t failure;
    uint64_t generation;
    uint64_t model_load_attempts;
    uint64_t context_init_attempts;
    uint64_t decode_attempts;
    char backend[NSK_LLAMA_RUNTIME_BACKEND_CAPACITY];
    char device[NSK_LLAMA_RUNTIME_DEVICE_CAPACITY];
};

/* trusted_runtime_directory is UTF-8 and must already be absolute. */
int32_t nsk_llama_runtime_configure(
    const char * trusted_runtime_directory,
    uint32_t explicit_retry,
    struct nsk_llama_runtime_status * out_status);

int32_t nsk_llama_runtime_status(struct nsk_llama_runtime_status * out_status);

#ifdef __cplusplus
}
#endif

#endif
