#ifndef RECORDER_SDK_H
#define RECORDER_SDK_H

#include <stdint.h>
#include <stddef.h>

#ifdef _WIN32
  #ifdef RECORDER_SDK_STATIC
    #define RECORDER_SDK_API
  #else
    #define RECORDER_SDK_API __declspec(dllimport)
  #endif
#else
  #define RECORDER_SDK_API
#endif

#ifdef __cplusplus
extern "C" {
#endif

#define RECORDER_SDK_OK 0
#define RECORDER_SDK_NULL_POINTER 1
#define RECORDER_SDK_INVALID_UTF8 2
#define RECORDER_SDK_INVALID_ARGUMENT 3
#define RECORDER_SDK_BUFFER_TOO_SMALL 4
#define RECORDER_SDK_ERROR 100

#define RECORDER_SDK_CAPTURE_KIND_DEFAULT 0
#define RECORDER_SDK_CAPTURE_KIND_INPUT 1
#define RECORDER_SDK_CAPTURE_KIND_LOOPBACK 2
#define RECORDER_SDK_CAPTURE_KIND_APP_OUTPUT 3

#define RECORDER_SDK_SAMPLE_FORMAT_DEFAULT 0
#define RECORDER_SDK_SAMPLE_FORMAT_F32 1
#define RECORDER_SDK_SAMPLE_FORMAT_I16 2

typedef struct RecorderCapture RecorderCapture;

typedef struct RecorderStartConfig {
    /* Windows only: "wasapi", "asio", "directsound", "waveout", or "dummy".
       Ignored on macOS/Linux. Null means platform default. */
    const char* audio_system;

    /* Optional device id from recorder_sdk_list_devices_json. Null means default device. */
    const char* device_id;

    /* Optional pre-processed/raw output path. At least one output path is required. */
    const char* raw_output_path;

    /* Optional post-processed output path. With no SDK processors this is equivalent to raw. */
    const char* processed_output_path;

    /* "wav", "flac", or "mp3". Null/empty infers from first output path, then defaults to "wav". */
    const char* output_format;

    /* 0 means use selected device default. */
    uint32_t sample_rate_hz;

    /* 0 means use selected device default. */
    uint16_t channels;

    /* 0 = selected device default, 1 = f32, 2 = i16. */
    int32_t sample_format;

    /* Optional speaker-output source id from recorder_sdk_list_capture_sources_json.
       Null disables secondary loopback capture; non-null requires loopback_output_path too. */
    const char* loopback_source_id;

    /* Where to write the secondary loopback recording. Required when loopback_source_id is set. */
    const char* loopback_output_path;

    /* Optional primary capture source id from recorder_sdk_list_capture_sources_json.
       When set, this supersedes device_id and may point at an input, loopback, or app-output source. */
    const char* source_id;

    /* 0 = infer / legacy behavior, 1 = input, 2 = loopback, 3 = app-output. */
    int32_t source_kind;
} RecorderStartConfig;

RECORDER_SDK_API const char* recorder_sdk_version(void);

/* Returns last error for the calling thread. Pointer remains valid until the next SDK call
   on that same thread. */
RECORDER_SDK_API const char* recorder_sdk_last_error(void);

/* Enumerates input devices as JSON.

   required_len_out receives the required byte count including the NUL terminator.
   If out_json is NULL or too small, returns RECORDER_SDK_BUFFER_TOO_SMALL.

   JSON shape:
   [
     {
       "id": "...",
       "name": "...",
       "default_format": {
         "sample_rate_hz": 48000,
         "channels": 2,
         "sample_format": "f32"
       }
     }
   ]
*/
RECORDER_SDK_API int recorder_sdk_list_devices_json(
    const char* audio_system,
    char* out_json,
    size_t out_json_len,
    size_t* required_len_out);

/* Enumerates all capture sources as JSON, including inputs, loopback sources, and app-output sources.

   required_len_out receives the required byte count including the NUL terminator.
   If out_json is NULL or too small, returns RECORDER_SDK_BUFFER_TOO_SMALL.
*/
RECORDER_SDK_API int recorder_sdk_list_capture_sources_json(
    const char* audio_system,
    char* out_json,
    size_t out_json_len,
    size_t* required_len_out);

RECORDER_SDK_API int recorder_sdk_start_recording(
    const RecorderStartConfig* config,
    RecorderCapture** out_capture);

/* Stops a recording. The handle remains valid and must still be freed. */
RECORDER_SDK_API int recorder_sdk_capture_stop(RecorderCapture* capture);

/* Stops if needed, then releases the handle. */
RECORDER_SDK_API void recorder_sdk_capture_free(RecorderCapture* capture);

#ifdef __cplusplus
}
#endif

#endif /* RECORDER_SDK_H */
