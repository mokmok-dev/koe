/* C ABI of the koe_speech_shim. The Swift implementation and the Rust
   bindings are both written against this file. */
#ifndef KOE_SPEECH_H
#define KOE_SPEECH_H

#include <stdint.h>

typedef struct koe_stt_string { const char *data; uint32_t length; } koe_stt_string;
typedef struct koe_stt_engine koe_stt_engine;
/* live = 1 asks for the tentative tail at the lowest latency; 0 keeps the
   configuration whose committed text is the most accurate. */
int32_t koe_stt_engine_create(const char *locale_utf8_or_null, int32_t live, koe_stt_engine **out);
int32_t koe_stt_engine_feed(koe_stt_engine *, const float *pcm, uint32_t frames);
/* 1 = popped one committed phrase, 0 = none, negative = status */
int32_t koe_stt_engine_take_committed(koe_stt_engine *, koe_stt_string *text, int64_t *start_ms, int64_t *end_ms);
/* 1 = the volatile tail changed since the last take, 0 = unchanged */
int32_t koe_stt_engine_take_volatile(koe_stt_engine *, koe_stt_string *text, uint32_t *revision);
int32_t koe_stt_engine_finish(koe_stt_engine *);   /* blocks until every result is in the mailbox */
void koe_stt_engine_destroy(koe_stt_engine *);

/* Strings the take functions hand out stay valid until the next call on the
   same handle. Status codes: 0 ok, -1 runtime older than macOS 26, -2
   unsupported locale, -3 missing or absent speech assets, -4 engine failure,
   -5 invalid argument. */
enum {
    koe_stt_ok = 0,
    koe_stt_err_version = -1,
    koe_stt_err_locale = -2,
    koe_stt_err_missing_model = -3,
    koe_stt_err_engine = -4,
    koe_stt_err_invalid = -5,
};

#endif
