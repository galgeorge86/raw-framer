/* Raw Framer: tiny C ABI over the Blackmagic RAW SDK (see .claude/ARCHITECTURE.md §3.1). */
#pragma once
#include <stdint.h>
#include <wchar.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct rf_braw rf_braw;

#define RF_OK 0
#define RF_ERR 1

#define RF_FORMAT_RGBA_F16 0
#define RF_FORMAT_RGB_F32 1

typedef struct {
    uint32_t width;
    uint32_t height;
    float fps;
    uint64_t frame_count;
} rf_braw_info_t;

typedef struct {
    uint64_t frame;
    uint32_t scale;             /* 1, 2, 4, 8 */
    uint32_t format;            /* RF_FORMAT_* */
    const char* gamma;          /* UTF-8, NULL = as shot */
    const char* gamut;          /* UTF-8, NULL = as shot */
    const char* post3dlut_mode; /* UTF-8, NULL = as shot */
    int32_t color_science_gen;  /* < 0 = as shot */
    int32_t highlight_recovery; /* < 0 = as shot, 0/1 */
    int32_t gamut_compression;  /* < 0 = as shot, 0/1 */
    int64_t iso;                /* < 0 = as shot */
    int64_t kelvin;             /* < 0 = as shot */
    int32_t has_tint;
    int32_t tint;
    int32_t has_exposure;
    float exposure;
} rf_braw_decode_t;

typedef struct {
    uint32_t width;
    uint32_t height;
    uint32_t format;
    uint8_t* data; /* free with rf_free */
    uint64_t size;
} rf_image_t;

int rf_braw_open(const wchar_t* clip_path, const wchar_t* runtime_dir, rf_braw** out);
void rf_braw_close(rf_braw* b);
int rf_braw_info(rf_braw* b, rf_braw_info_t* out);
/* JSON: {"version","camera_type","metadata":{..},"processing":{..},"lists":{..}} */
int rf_braw_clip_json(rf_braw* b, char** json_out);
/* JSON: {"timecode","metadata":{..},"processing":{..},"ranges":{..}} */
int rf_braw_frame_json(rf_braw* b, uint64_t frame, char** json_out);
/* JSON array of gamma names available for a colour science generation (< 0 = as shot). */
int rf_braw_gamma_list(rf_braw* b, int32_t gen, char** json_out);
int rf_braw_decode(rf_braw* b, const rf_braw_decode_t* params, rf_image_t* out);

/* Sidecar (.sidecar next to the clip, shared with Resolve and Blackmagic RAW Player).
   frame < 0 sets clip metadata, otherwise that frame's. `value` is text converted to the
   key's existing type; NULL clears the override (camera original). Nothing is written to
   disk until rf_braw_save_sidecar. */
int rf_braw_set_metadata(rf_braw* b, int64_t frame, const char* key, const char* value);
int rf_braw_save_sidecar(rf_braw* b);
int rf_braw_reload_sidecar(rf_braw* b);
int rf_braw_sidecar_attached(rf_braw* b, int* attached);

/* Decode pipelines. JSON array [{"id":"cpu"|"cuda"|"opencl","name":".."}] of those with a
   usable device. set_pipeline applies to clips opened afterwards (reopen clips to switch);
   a GPU pipeline that can't start falls back to CPU, which active_pipeline then reports. */
int rf_braw_pipelines(const wchar_t* runtime_dir, char** json_out);
int rf_braw_set_pipeline(const char* id);
int rf_braw_active_pipeline(char** out);

typedef struct {
    uint32_t sample_rate;
    uint32_t channels;
    uint32_t bit_depth;   /* PCM little endian, signed */
    uint64_t sample_count; /* sample frames */
} rf_braw_audio_info_t;

/* RF_ERR when the clip has no audio. */
int rf_braw_audio_info(rf_braw* b, rf_braw_audio_info_t* out);
/* Reads up to max_frames sample frames starting at `start` as raw interleaved PCM. */
int rf_braw_audio_read(rf_braw* b, uint64_t start, uint32_t max_frames, void* buf, uint32_t buf_bytes, uint32_t* frames_read);
void rf_free(void* p);
const char* rf_braw_last_error(void);

#ifdef __cplusplus
}
#endif
