#ifndef KARAOK_FFI_H
#define KARAOK_FFI_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/**
 * Opaque handle owning the Rust AppController + real-time audio engine.
 * Created via `karaok_controller_create` and released via `karaok_controller_destroy`.
 * Never dereferenced from Swift/C; thread-safe for control-plane & lock-free snapshot reads.
 */
typedef struct KaraokControllerHandle KaraokControllerHandle;

/**
 * Lock-free real-time transport snapshot published by the Rust master output clock.
 * Read from SwiftUI without heap allocation or JSON parsing.
 */
typedef struct KaraokTransportSnapshotC {
    uint64_t generation;
    uint8_t state;                  /* 0=Stopped, 1=Playing, 2=Paused, 3=Buffering, 4=RouteLost */
    uint8_t presentation_quality;   /* 0=BackendTimestamp, 1=OutputCursorCompensated, 2=Uncertain */
    uint8_t using_stems;
    uint8_t stems_available;
    double source_position_s;
    double duration_s;
    uint64_t monotonic_host_ns;
    double playback_ratio;
    double presentation_latency_ms;
} KaraokTransportSnapshotC;

/**
 * Lock-free real-time vocal meter & pitch-correction snapshot.
 */
typedef struct KaraokMeterSnapshotC {
    float input_pre_peak;
    float input_pre_rms;
    uint8_t input_clipped;
    uint8_t output_clipped;
    uint8_t voiced;
    uint8_t active_mic_capture;
    float voice_post_peak;
    float voice_post_rms;
    float output_peak;
    float detected_f0_hz;
    float target_f0_hz;
    float pitch_confidence;
    float pitch_correction_cents;
} KaraokMeterSnapshotC;

/** Returns FFI ABI version (currently 1). Swift must verify equality before calling other symbols. */
uint32_t karaok_abi_version(void);

/**
 * Create a new controller instance.
 * If `app_support_dir` or `cache_dir` is NULL, resolves standard macOS Application Support / Caches paths.
 * Returns NULL on fatal initialization error (caller can inspect `out_error_json` if non-NULL; free with `karaok_string_free`).
 */
KaraokControllerHandle* karaok_controller_create(
    const char* app_support_dir,
    const char* cache_dir,
    char** out_error_json
);

/** Destroy controller cleanly, stopping all audio streams and worker threads. Safe with NULL. */
void karaok_controller_destroy(KaraokControllerHandle* handle);

/**
 * Dispatch a JSON-encoded `ControllerCommand` and receive a newly allocated UTF-8 JSON `ControllerResponse`.
 * Caller MUST free the returned pointer with `karaok_string_free`.
 */
char* karaok_controller_dispatch_json(
    KaraokControllerHandle* handle,
    const char* command_json
);

/**
 * Drain queued `AppEvent` items as a UTF-8 JSON array.
 * Caller MUST free the returned pointer with `karaok_string_free`.
 */
char* karaok_controller_poll_events_json(KaraokControllerHandle* handle);

/** Lock-free read of current transport snapshot. Returns 1 on success, 0 on invalid pointer. */
int32_t karaok_controller_read_transport(
    KaraokControllerHandle* handle,
    KaraokTransportSnapshotC* out_snapshot
);

/** Lock-free read of current meters & live pitch telemetry. Returns 1 on success, 0 on invalid pointer. */
int32_t karaok_controller_read_meters(
    KaraokControllerHandle* handle,
    KaraokMeterSnapshotC* out_snapshot
);

/** Free any UTF-8 C string returned by `karaok_controller_*`. Safe with NULL. */
void karaok_string_free(char* ptr);

#ifdef __cplusplus
}
#endif

#endif /* KARAOK_FFI_H */
