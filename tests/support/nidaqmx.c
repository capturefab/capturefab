/*
 * Independent Linux NI-DAQmx ABI fixture. No NI headers, SDK or product imports.
 * Compile as a private shared library with a C11 compiler. Scenario is the first
 * physical device prefix, e.g. Success/ai0. Only the trace path is environment.
 * Core ABI/raw words were frozen in fixtures/nidaqmx/abi.json before native code.
 */
#define _POSIX_C_SOURCE 200809L
#include <errno.h>
#include <fcntl.h>
#include <float.h>
#include <inttypes.h>
#include <limits.h>
#include <math.h>
#include <signal.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

typedef void *TaskHandle;
typedef int32_t int32;
typedef uint32_t uInt32;
typedef uint64_t uInt64;
typedef uint32_t bool32;
typedef double float64;
_Static_assert(CHAR_BIT == 8 && sizeof(int32) == 4 && sizeof(uInt32) == 4, "32-bit ABI");
_Static_assert(sizeof(uInt64) == 8 && sizeof(float64) == 8, "64-bit ABI");
_Static_assert(DBL_MANT_DIG == 53 && DBL_MAX_EXP == 1024, "IEEE binary64");
#define API __attribute__((visibility("default")))
/* Family-only builds hide unselected NI symbols from the dynamic interface.
 * Bodies remain compiled to keep old validation/word tables independently intact. */
#if defined(NIDAQMX_FIXTURE_INPUT_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_CI_ONLY)
#define AI_API __attribute__((visibility("hidden")))
#else
#define AI_API API
#endif
#if defined(NIDAQMX_FIXTURE_FIVE_ONLY) || defined(NIDAQMX_FIXTURE_RESISTIVE_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_ONLY) || defined(NIDAQMX_FIXTURE_AI_ONLY) || defined(NIDAQMX_FIXTURE_COUNT_ONLY) || defined(NIDAQMX_FIXTURE_FREQUENCY_ONLY)
#define DI_API __attribute__((visibility("hidden")))
#else
#define DI_API API
#endif
#if defined(NIDAQMX_FIXTURE_FIVE_ONLY) || defined(NIDAQMX_FIXTURE_RESISTIVE_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_ONLY) || defined(NIDAQMX_FIXTURE_AI_ONLY) || defined(NIDAQMX_FIXTURE_DIGITAL_ONLY) || defined(NIDAQMX_FIXTURE_FREQUENCY_ONLY)
#define COUNT_API __attribute__((visibility("hidden")))
#else
#define COUNT_API API
#endif
#if defined(NIDAQMX_FIXTURE_FIVE_ONLY) || defined(NIDAQMX_FIXTURE_RESISTIVE_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_ONLY) || defined(NIDAQMX_FIXTURE_AI_ONLY) || defined(NIDAQMX_FIXTURE_DIGITAL_ONLY) || defined(NIDAQMX_FIXTURE_COUNT_ONLY)
#define FREQ_API __attribute__((visibility("hidden")))
#else
#define FREQ_API API
#endif
#if defined(NIDAQMX_FIXTURE_FIVE_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_ONLY) || defined(NIDAQMX_FIXTURE_RESISTIVE_ONLY)
#define OLD_AI_API __attribute__((visibility("hidden")))
#else
#define OLD_AI_API AI_API
#endif
#if defined(NIDAQMX_FIXTURE_FIVE_ONLY) || defined(NIDAQMX_FIXTURE_OMIT_STRAIN) || defined(NIDAQMX_FIXTURE_RESISTIVE_ONLY) || defined(NIDAQMX_FIXTURE_INPUT_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_CI_ONLY)
#define STRAIN_API __attribute__((visibility("hidden")))
#else
#define STRAIN_API API
#endif
#if defined(NIDAQMX_FIXTURE_FIVE_ONLY) || defined(NIDAQMX_FIXTURE_OMIT_BRIDGE) || defined(NIDAQMX_FIXTURE_RESISTIVE_ONLY) || defined(NIDAQMX_FIXTURE_INPUT_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_CI_ONLY)
#define BRIDGE_API __attribute__((visibility("hidden")))
#else
#define BRIDGE_API API
#endif
#if defined(NIDAQMX_FIXTURE_FIVE_ONLY) || defined(NIDAQMX_FIXTURE_OMIT_ACCEL) || defined(NIDAQMX_FIXTURE_RESISTIVE_ONLY) || defined(NIDAQMX_FIXTURE_INPUT_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_CI_ONLY)
#define ACCEL_API __attribute__((visibility("hidden")))
#else
#define ACCEL_API API
#endif
#if defined(NIDAQMX_FIXTURE_OMIT_COUPLING) || defined(NIDAQMX_FIXTURE_INPUT_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_CI_ONLY)
#define COUPLING_API __attribute__((visibility("hidden")))
#else
#define COUPLING_API API
#endif
#define CONTRACT INT32_C(-990001)
#define MAX_CHANNELS 64
#define MAX_NAME 256
#define MAX_CALLS 4096
#define MAX_TRACE_BYTES (1024 * 1024)

typedef struct {
    uInt32 channels, named, pending_name, reads, input_buffer, last_available, tc_pending_setter;
    int measurement; /* 0 unconfigured, 1 voltage, 2 thermocouple, 3 current, 4 RTD, 5 mixed, 6 mixed voltage overrides; 7 DI, 8 count, 9 frequency; 10 strain, 11 ratio, 12 acceleration, 13 Mixed7, 14 period, 15 pulse, 16 angular, 17 linear; 18 resistance, 19 Iex, 20 Vex, 21 force, 22 pressure, 23 torque, 24 Mixed13. */
    int timing, overwrite, counted, started, rate_queried, available_queried, terminal_read, stop_attempted, stopped;
    uInt64 hint, scans_read;
    float64 rate;
    char channel[MAX_CHANNELS][MAX_NAME + 1];
    char requested[MAX_CHANNELS][MAX_NAME + 1];
    uInt32 width[MAX_CHANNELS], terminal_set, coupling_set[MAX_CHANNELS];
    int32 coupling_expected[MAX_CHANNELS];
    uint64_t width_mask;
} Task;

static Task *owned;
static unsigned created;
static unsigned input_profile;
static unsigned calls;
static uint64_t sequence;
static int trace_fd = -2;
static char scenario[MAX_NAME + 1] = "Success";
static _Thread_local int32 last_status;
static _Thread_local int slow_diagnostic;
#ifdef NIDAQMX_FIXTURE_SLOW_DIAGNOSTICS
static int slow_once = 1;
#endif

static const uint64_t words[12] = {
    UINT64_C(0x3ff0000000000000), UINT64_C(0xc004000000000000),
    UINT64_C(0x8000000000000000), UINT64_C(0x7ff0000000000000),
    UINT64_C(0xfff0000000000000), UINT64_C(0x7ff80000000000a5),
    UINT64_C(0x0000000000000001), UINT64_C(0x7fefffffffffffff),
    UINT64_C(0x0000000000000000), UINT64_C(0x0010000000000000),
    UINT64_C(0x8000000000000001), UINT64_C(0xfff8000000000045)
};
/* Independent thermocouple oracle: no unit conversion, including C/F 25.0. */
static const uint64_t tc_words[12] = {
    UINT64_C(0x4039000000000000), UINT64_C(0x4039000000000000),
    UINT64_C(0x8000000000000000), UINT64_C(0x7ff80000000000a5),
    UINT64_C(0x7ff0000000000000), UINT64_C(0xfff0000000000000),
    UINT64_C(0x0000000000000001), UINT64_C(0x7fefffffffffffff),
    UINT64_C(0x0000000000000000), UINT64_C(0x0010000000000000),
    UINT64_C(0x8000000000000001), UINT64_C(0xfff8000000000045)
};
/* Current oracle frozen before implementation: no V/R or milliamp conversion. */
static const uint64_t current_words[12] = {
    UINT64_C(0x3f90000000000000), UINT64_C(0x3f90000000000000),
    UINT64_C(0x3f70624dd2f1a9fc), UINT64_C(0x3f947ae147ae147b),
    UINT64_C(0x8000000000000000), UINT64_C(0x7ff80000000000a5),
    UINT64_C(0x7ff0000000000000), UINT64_C(0xfff0000000000000),
    UINT64_C(0x0000000000000001), UINT64_C(0x7fefffffffffffff),
    UINT64_C(0x8000000000000001), UINT64_C(0xfff8000000000045)
};
/* Frozen RTD oracle5f83: same 25.0 under C/F is not converted locally. */
static const uint64_t rtd_words[12] = {
    UINT64_C(0x4039000000000000), UINT64_C(0x4039000000000000),
    UINT64_C(0x8000000000000000), UINT64_C(0x7ff80000000000a5),
    UINT64_C(0x7ff0000000000000), UINT64_C(0xfff0000000000000),
    UINT64_C(0x0000000000000001), UINT64_C(0x7fefffffffffffff),
    UINT64_C(0x0000000000000000), UINT64_C(0x0010000000000000),
    UINT64_C(0x8000000000000001), UINT64_C(0xfff8000000000045)
};
/* Independent mixed oracle d4a74d1e, two scans by four actual channels. */
static const uint64_t mixed_words[8] = {
    UINT64_C(0x3ff0000000000000), UINT64_C(0x4039000000000000),
    UINT64_C(0x3f90000000000000), UINT64_C(0x4039000000000000),
    UINT64_C(0x8000000000000000), UINT64_C(0x7ff80000000000a5),
    UINT64_C(0x7ff0000000000000), UINT64_C(0xc004000000000000)
};
static const uint64_t resistive_words[12] = {
    UINT64_C(0x4039000000000000),
    UINT64_C(0x4039000000000000),
    UINT64_C(0x8000000000000000),
    UINT64_C(0x7ff80000000000a5),
    UINT64_C(0x7ff0000000000000),
    UINT64_C(0xfff0000000000000),
    UINT64_C(0x3f90000000000000),
    UINT64_C(0xbf80000000000000),
    UINT64_C(0x0000000000000000),
    UINT64_C(0x4080000000000000),
    UINT64_C(0x0000000000000001),
    UINT64_C(0xfff8000000000045),
};
static const uint64_t physical_bridge_words[12] = {
    UINT64_C(0x3f90000000000000),
    UINT64_C(0x3f90000000000000),
    UINT64_C(0x0000000000000000),
    UINT64_C(0x8000000000000000),
    UINT64_C(0x7ff0000000000000),
    UINT64_C(0xfff0000000000000),
    UINT64_C(0x7ff80000000000a5),
    UINT64_C(0xc004000000000000),
    UINT64_C(0x4039000000000000),
    UINT64_C(0x4060000000000000),
    UINT64_C(0x0000000000000001),
    UINT64_C(0xfff8000000000045),
};
static const uint64_t resistive_mixed_words[26] = {
    UINT64_C(0x3ff0000000000000),
    UINT64_C(0x4039000000000000),
    UINT64_C(0x3f90000000000000),
    UINT64_C(0x4039000000000000),
    UINT64_C(0x3f50000000000000),
    UINT64_C(0x3f90000000000000),
    UINT64_C(0x3ff0000000000000),
    UINT64_C(0x4039000000000000),
    UINT64_C(0x4039000000000000),
    UINT64_C(0x4039000000000000),
    UINT64_C(0x3f90000000000000),
    UINT64_C(0x3f90000000000000),
    UINT64_C(0x3f90000000000000),
    UINT64_C(0x8000000000000000),
    UINT64_C(0x7ff80000000000a5),
    UINT64_C(0x7ff0000000000000),
    UINT64_C(0xc004000000000000),
    UINT64_C(0xbf80000000000000),
    UINT64_C(0xfff0000000000000),
    UINT64_C(0x0000000000000000),
    UINT64_C(0x8000000000000000),
    UINT64_C(0x7ff0000000000000),
    UINT64_C(0x7ff80000000000a5),
    UINT64_C(0xc004000000000000),
    UINT64_C(0xfff0000000000000),
    UINT64_C(0x0000000000000000),
};


/* Additive independently frozen NI-five raw words879f/0032. */
static const uint64_t dynamic_words[12] = {
    UINT64_C(0x4039000000000000), UINT64_C(0x4039000000000000),
    UINT64_C(0x8000000000000000), UINT64_C(0x0000000000000000),
    UINT64_C(0x3f90000000000000), UINT64_C(0xbfd8000000000000),
    UINT64_C(0x7ff8000000000042), UINT64_C(0x7ff0000000000000),
    UINT64_C(0xfff0000000000000), UINT64_C(0x0000000000000001),
    UINT64_C(0x8000000000000001), UINT64_C(0x3ff0000000000000)
};
static const uint64_t position_words[12] = {
    UINT64_C(0x3f90000000000000), UINT64_C(0x3f90000000000000),
    UINT64_C(0x8000000000000000), UINT64_C(0x0000000000000000),
    UINT64_C(0xbfda000000000000), UINT64_C(0x3feb000000000000),
    UINT64_C(0x7ff0000000000000), UINT64_C(0x7ff80000000000a5),
    UINT64_C(0xfff0000000000000), UINT64_C(0x0000000000000001),
    UINT64_C(0x8000000000000001), UINT64_C(0xbff0000000000000)
};
static const uint64_t five_mixed_words[36] = {
    UINT64_C(0x3ff0000000000000), UINT64_C(0x4039000000000000),
    UINT64_C(0x3f90000000000000), UINT64_C(0x4039000000000000),
    UINT64_C(0x3f50000000000000), UINT64_C(0x3f90000000000000),
    UINT64_C(0x3ff0000000000000), UINT64_C(0x4039000000000000),
    UINT64_C(0x4039000000000000), UINT64_C(0x4039000000000000),
    UINT64_C(0x3f90000000000000), UINT64_C(0x3f90000000000000),
    UINT64_C(0x3f90000000000000), UINT64_C(0x4039000000000000),
    UINT64_C(0x4039000000000000), UINT64_C(0x4039000000000000),
    UINT64_C(0x3f90000000000000), UINT64_C(0x3f90000000000000),
    UINT64_C(0x8000000000000000), UINT64_C(0x7ff80000000000a5),
    UINT64_C(0x7ff0000000000000), UINT64_C(0xc004000000000000),
    UINT64_C(0xbf80000000000000), UINT64_C(0xfff0000000000000),
    UINT64_C(0x0000000000000000), UINT64_C(0x8000000000000000),
    UINT64_C(0x7ff0000000000000), UINT64_C(0x7ff80000000000a5),
    UINT64_C(0xc004000000000000), UINT64_C(0xfff0000000000000),
    UINT64_C(0x0000000000000000), UINT64_C(0x8000000000000000),
    UINT64_C(0x7ff8000000000042), UINT64_C(0x7ff0000000000000),
    UINT64_C(0xbfda000000000000), UINT64_C(0x3feb000000000000)
};

static uint64_t bits(float64 value) {
    uint64_t result;
    memcpy(&result, &value, sizeof(result));
    return result;
}
static uint64_t now_ns(void) {
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) return 0;
    return (uint64_t)ts.tv_sec * UINT64_C(1000000000) + (uint64_t)ts.tv_nsec;
}
static void delay_ms(unsigned milliseconds) {
    struct timespec ts = {(time_t)(milliseconds / 1000), (long)(milliseconds % 1000) * 1000000L};
    while (nanosleep(&ts, &ts) != 0 && errno == EINTR) {}
}
static int five_mixed(void) { return strncmp(scenario,"FiveMixed",9)==0; }
static int resistive_mixed(void) { return strncmp(scenario,"ResistiveMixed",14)==0 || five_mixed(); }
static int engineering_mixed(void) { return strncmp(scenario, "EngineeringMixed", 16) == 0 || resistive_mixed(); }
static int mixed_mode(void) { return strncmp(scenario, "Mixed", 5) == 0 || engineering_mixed(); }
static int mode(const char *name) {
    const char *value = scenario;
    if (five_mixed()) value += 9;
    else if (resistive_mixed()) value += 14;
    else if (engineering_mixed()) value += 16;
    else if (mixed_mode()) value += 5;
    else if(strncmp(value,"Velocity",8)==0)value+=8;
    else if(strncmp(value,"IepeForce",9)==0)value+=9;
    else if(strncmp(value,"Charge",6)==0)value+=6;
    else if(strncmp(value,"Lvdt",4)==0)value+=4;
    else if(strncmp(value,"Rvdt",4)==0)value+=4;
    else if(strncmp(value,"Resistance",10)==0)value+=10;
    else if(strncmp(value,"ThermIex",8)==0)value+=8;
    else if(strncmp(value,"ThermVex",8)==0)value+=8;
    else if(strncmp(value,"Force",5)==0)value+=5;
    else if(strncmp(value,"Pressure",8)==0)value+=8;
    else if(strncmp(value,"Torque",6)==0)value+=6;
    else if (strncmp(value, "Strain", 6) == 0) value += 6;
    else if (strncmp(value, "Ratio", 5) == 0) value += 5;
    else if (strncmp(value, "Accel", 5) == 0) value += 5;
    else if (strncmp(value, "Period", 6) == 0) value += 6;
    else if (strncmp(value, "Pulse", 5) == 0) value += 5;
    else if (strncmp(value, "Angle", 5) == 0) value += 5;
    else if (strncmp(value, "Linear", 6) == 0) value += 6;
    else if (input_profile && strncmp(value, "Digital", 7) == 0) value += 7;
    else if (input_profile && strncmp(value, "Count", 5) == 0) value += 5;
    else if (input_profile && strncmp(value, "Frequency", 9) == 0) value += 9;
    return strcmp(value, name) == 0;
}
static void trace(const char *fn, const char *phase, uint64_t id, const char *format, ...) {
    char detail[3072], line[4096];
    va_list args;
    va_start(args, format);
    int extra = vsnprintf(detail, sizeof(detail), format, args);
    va_end(args);
    if (extra < 0 || (size_t)extra >= sizeof(detail)) return;
    if (trace_fd == -2) {
        const char *path = getenv("CAPTUREFAB_NIDAQMX_FIXTURE_LOG");
        trace_fd = -1;
        if (path && path[0] == '/' && strnlen(path, 4097) <= 4096) {
            trace_fd = open(path, O_WRONLY | O_CREAT | O_APPEND | O_CLOEXEC | O_NOFOLLOW, 0600);
        }
    }
    if (trace_fd < 0) return;
    struct stat st;
    if (fstat(trace_fd, &st) != 0 || !S_ISREG(st.st_mode) || st.st_nlink != 1 ||
        st.st_size > MAX_TRACE_BYTES) return;
    int length = snprintf(line, sizeof(line),
        "{\"fn\":\"%s\",\"phase\":\"%s\",\"pid\":%ld,\"seq\":%" PRIu64
        ",\"monotonic_ns\":%" PRIu64 ",\"clock\":\"CLOCK_MONOTONIC\",\"task\":%u%s}\n",
        fn, phase, (long)getpid(), id, now_ns(), owned ? 1U : 0U, detail);
    if (length <= 0 || (size_t)length >= sizeof(line) ||
        (uint64_t)st.st_size + (unsigned)length > MAX_TRACE_BYTES) return;
    size_t done = 0;
    while (done < (size_t)length) {
        ssize_t written = write(trace_fd, line + done, (size_t)length - done);
        if (written > 0) done += (size_t)written;
        else if (written < 0 && errno == EINTR) continue;
        else break;
    }
}
static uint64_t enter(const char *fn) {
    uint64_t id = ++sequence;
    ++calls;
    trace(fn, "enter", id, "");
    return id;
}
static int32 result(const char *fn, uint64_t id, int32 status) {
    last_status = status;
    trace(fn, "return", id, ",\"status\":%" PRId32, status);
    return status;
}
static int32 bad(const char *fn, uint64_t id, const char *reason) {
    last_status = CONTRACT;
    trace(fn, "contract", id, ",\"status\":%" PRId32 ",\"reason\":\"%s\"", CONTRACT, reason);
    return CONTRACT;
}
static int valid(TaskHandle handle) {
    return calls <= MAX_CALLS && owned != NULL && handle == (TaskHandle)owned;
}
static int empty(const char *text) { return text && text[0] == '\0'; }
static int physical(const char *name, char *device) {
    if (!name || strnlen(name, MAX_NAME + 1) > MAX_NAME) return 0;
    const char *p = name;
    if (*p == '/') ++p;
    const char *slash = strchr(p, '/');
    if (!slash || slash == p || strncmp(slash + 1, "ai", 2) != 0 || slash[3] == '\0') return 0;
    size_t length = (size_t)(slash - p);
    for (const char *q = p; q < slash; ++q)
        if (!((*q >= 'A' && *q <= 'Z') || (*q >= 'a' && *q <= 'z') ||
              (*q >= '0' && *q <= '9') || *q == '_')) return 0;
    for (const char *q = slash + 3; *q; ++q) if (*q < '0' || *q > '9') return 0;
    memcpy(device, p, length); device[length] = '\0';
    return 1;
}

/* Only the explicit Mixed* producer scenarios use this per-column table.
 * Legacy homogeneous constructor validations below remain independently fixed. */
typedef struct {
    int kind;
    int32 terminal, units, type, source, wire;
    float64 minimum, maximum, a, b;
    int null_scale, empty_aux;
} MixedArgs;
static int mixed_physical(const char *name) {
    char device[MAX_NAME + 1];
    return physical(name, device) && (strncmp(device, "Mixed", 5) == 0 || strncmp(device, "EngineeringMixed", 16) == 0 || strncmp(device,"ResistiveMixed",14)==0 || strncmp(device,"FiveMixed",9)==0);
}
static int32 mixed_add(const char *fn, uint64_t id, TaskHandle handle,
        const char *name, const char *virtual_name, const MixedArgs *a) {
    char device[MAX_NAME + 1], suffix[32];
    if (!valid(handle) || owned->timing || owned->started || owned->counted ||
        owned->tc_pending_setter || !physical(name, device) || !empty(virtual_name))
        return bad(fn, id, "one fresh mixed task and immediate optional TC setter order");
    if (owned->channels == 0) strcpy(scenario, device);
    else if (strcmp(scenario, device) != 0)
        return bad(fn, id, "one mixed physical-device scenario");
    int voltage_only = mode("Voltage");
    uInt32 index = owned->channels;
    if (index >= (voltage_only ? 2U : 4U) ||
        a->kind != (voltage_only ? 1 : (int)index + 1))
        return bad(fn, id, "frozen ordered V/TC/current/RTD union or two-voltage override");
    snprintf(suffix, sizeof(suffix), "ai%" PRIu32, index);
    if (strcmp(strrchr(name, '/') + 1, suffix) != 0)
        return bad(fn, id, "explicit mixed ordinal matches individual physical name");
    trace(fn, "arguments", id,
        ",\"physical\":\"%s\",\"virtual_empty\":true,\"column_index\":%" PRIu32
        ",\"constructor_kind\":%d,\"terminal\":%" PRId32 ",\"units\":%" PRId32
        ",\"type\":%" PRId32 ",\"source\":%" PRId32 ",\"wire\":%" PRId32
        ",\"minimum_bits\":\"%016" PRIx64 "\",\"maximum_bits\":\"%016" PRIx64
        "\",\"arg_a_bits\":\"%016" PRIx64 "\",\"arg_b_bits\":\"%016" PRIx64
        "\",\"custom_scale_null\":%s,\"aux_empty\":%s",
        name, index, a->kind, a->terminal, a->units, a->type, a->source, a->wire,
        bits(a->minimum), bits(a->maximum), bits(a->a), bits(a->b),
        a->null_scale ? "true" : "false", a->empty_aux ? "true" : "false");
    int good = 0;
    switch (a->kind) {
    case 1:
        good = a->terminal == (index == 0 ? 10106 : 10083) && a->units == 10348 && a->null_scale &&
            bits(a->minimum) == (index == 0 ? UINT64_C(0xc024000000000000) : UINT64_C(0xc004000000000000)) &&
            bits(a->maximum) == (index == 0 ? UINT64_C(0x4024000000000000) : UINT64_C(0x4004000000000000));
        break;
    case 2:
        good = a->units == 10143 && a->type == 10073 && a->source == 10200 && a->empty_aux &&
            bits(a->minimum) == UINT64_C(0xc049000000000000) &&
            bits(a->maximum) == UINT64_C(0x407f400000000000) && bits(a->a) == 0;
        break;
    case 3:
        good = a->terminal == 10078 && a->units == 10342 && a->source == 10167 && a->null_scale &&
            bits(a->minimum) == UINT64_C(0xbfa0000000000000) &&
            bits(a->maximum) == UINT64_C(0x3fa0000000000000) &&
            bits(a->a) == UINT64_C(0x406f400000000000);
        break;
    case 4:
        good = a->units == 10144 && a->type == 12483 && a->wire == 4 && a->source == 10167 &&
            bits(a->minimum) == UINT64_C(0xc044000000000000) &&
            bits(a->maximum) == UINT64_C(0x4072c00000000000) &&
            bits(a->a) == UINT64_C(0x3f60000000000000) &&
            bits(a->b) == UINT64_C(0x408f400000000000);
        break;
    }
    if (!good) return bad(fn, id, "frozen mixed constructor slot/enum/pointer/scientific bits");
    owned->measurement = engineering_mixed() ? 13 : voltage_only ? 6 : 5;
    if (!engineering_mixed() && mode("LaterFail") && index == 3)
        return result(fn, id, INT32_C(-990104));
    strcpy(owned->channel[owned->channels++], name);
    if (a->kind == 2 && (mode("TCExplicit") || mode("TCTermError")))
        owned->tc_pending_setter = owned->channels;
    return result(fn, id, mode("SetupWarn") && owned->channels == 1 ? 12345 : 0);
}


/* Additive889c/1a3e literal controls; no NI headers or product values. */
static const uint64_t engineering_words[12] = {
    UINT64_C(0x3f90000000000000), UINT64_C(0x3f90000000000000), UINT64_C(0x3f90000000000000),
    UINT64_C(0x8000000000000000), UINT64_C(0x7ff0000000000000), UINT64_C(0xfff0000000000000),
    UINT64_C(0x7ff80000000000a5), UINT64_C(0x0000000000000000), UINT64_C(0x3f50000000000000),
    UINT64_C(0xbf80000000000000), UINT64_C(0xc004000000000000), UINT64_C(0xfff8000000000045)
};
static const uint64_t engineering_mixed_words[14] = {
    UINT64_C(0x3ff0000000000000), UINT64_C(0x4039000000000000), UINT64_C(0x3f90000000000000),
    UINT64_C(0x4039000000000000), UINT64_C(0x3f50000000000000), UINT64_C(0x3f90000000000000),
    UINT64_C(0x3ff0000000000000), UINT64_C(0x8000000000000000), UINT64_C(0x7ff80000000000a5),
    UINT64_C(0x7ff0000000000000), UINT64_C(0xc004000000000000), UINT64_C(0xbf80000000000000),
    UINT64_C(0xfff0000000000000), UINT64_C(0x0000000000000000)
};
static const uint64_t engineering_counter_words[8] = {
    UINT64_C(0x3f90000000000000), UINT64_C(0x0000000000000000), UINT64_C(0x8000000000000000),
    UINT64_C(0x4039000000000000), UINT64_C(0x7ff80000000000a5), UINT64_C(0x7ff0000000000000),
    UINT64_C(0xfff0000000000000), UINT64_C(0xc004000000000000)
};
static int32 engineering_add(const char *fn, uint64_t id, TaskHandle handle,
        const char *name, const char *assigned, int kind) {
    char device[MAX_NAME + 1], suffix[32];
    if (!valid(handle) || owned->timing || owned->started || owned->counted || owned->tc_pending_setter ||
        !physical(name, device) || !empty(assigned))
        return bad(fn, id, "fresh engineering constructor in owned setup");
    if (!owned->channels) strcpy(scenario, device);
    if (strcmp(scenario, device)) return bad(fn, id, "one explicit engineering scenario device");
    uInt32 index = owned->channels;
    unsigned limit = five_mixed() ? 18U : resistive_mixed() ? 13U : engineering_mixed() ? 7U : kind == 10 && mode("All") ? 7U : kind == 11 && mode("All") ? 3U : kind == 12 ? 3U : 2U;
    if (index >= limit || (engineering_mixed() ? index < 4 || kind != (int)index + 6 : owned->measurement && owned->measurement != kind))
        return bad(fn, id, "ordered finite engineering family/union");
    snprintf(suffix, sizeof(suffix), "ai%" PRIu32, index);
    if (strcmp(strrchr(name, '/') + 1, suffix)) return bad(fn, id, "ordered engineering physical suffix");
    owned->measurement = engineering_mixed() ? 13 : kind;
    return 0;
}
static int32 engineering_commit(const char *fn, uint64_t id, const char *name, int32 coupling) {
    if (mode("LaterFail") && owned->channels == (five_mixed()?17U:resistive_mixed()?12U:6U)) return result(fn, id, INT32_C(-990104));
    uInt32 i = owned->channels;
    strcpy(owned->channel[i], name); ++owned->channels;
    owned->coupling_expected[i] = mode("Omitted") ? 0 : coupling;
    return result(fn, id, 0);
}
STRAIN_API int32 DAQmxCreateAIStrainGageChan(TaskHandle handle, const char *name, const char *assigned,
        float64 minimum, float64 maximum, int32 units, int32 bridge, int32 source, float64 volts,
        float64 gage, float64 initial, float64 ohms, float64 poisson, float64 lead, const char *scale) {
    uint64_t id = enter("DAQmxCreateAIStrainGageChan");
    int32 setup = engineering_add("DAQmxCreateAIStrainGageChan", id, handle, name, assigned, 10);
    if (setup) return setup;
    uInt32 i = engineering_mixed() ? 0 : owned->channels; unsigned b = i % 2;
    const int32 layouts[7] = {10183,10184,10185,10188,10189,10271,10272};
    int32 expected_bridge = mode("All") ? layouts[i] : b ? 10185 : 10188;
    uint64_t expected_volts = b ? UINT64_C(0x4014000000000000) : UINT64_C(0x4004000000000000);
    int32 expected_source = b ? 10167 : 10200;
    if (mode("None")) { expected_source = 10230; expected_volts = UINT64_C(0x3ff4000000000000); }
    if (mode("NegativeZero")) { expected_source = 10230; expected_volts = UINT64_C(0x8000000000000000); }
    trace("DAQmxCreateAIStrainGageChan", "arguments", id,
        ",\"physical\":\"%s\",\"virtual_empty\":true,\"minimum_bits\":\"%016" PRIx64 "\",\"maximum_bits\":\"%016" PRIx64
        "\",\"units\":%" PRId32 ",\"bridge\":%" PRId32 ",\"excitation_source\":%" PRId32
        ",\"excitation_bits\":\"%016" PRIx64 "\",\"gage_bits\":\"%016" PRIx64 "\",\"initial_bits\":\"%016" PRIx64
        "\",\"ohms_bits\":\"%016" PRIx64 "\",\"poisson_bits\":\"%016" PRIx64 "\",\"lead_bits\":\"%016" PRIx64 "\",\"custom_scale_null\":%s",
        name,bits(minimum),bits(maximum),units,bridge,source,bits(volts),bits(gage),bits(initial),bits(ohms),bits(poisson),bits(lead),scale ? "false":"true");
    if (units != 10299 || bridge != expected_bridge || source != expected_source || bits(volts) != expected_volts || scale ||
        bits(minimum) != (b ? UINT64_C(0xbf80000000000000) : UINT64_C(0xbf90000000000000)) ||
        bits(maximum) != (b ? UINT64_C(0x3fa0000000000000) : UINT64_C(0x3f98000000000000)) ||
        bits(gage) != (b ? UINT64_C(0x4002000000000000) : UINT64_C(0x4000000000000000)) ||
        bits(initial) != (b ? UINT64_C(0xbf60000000000000) : UINT64_C(0x3f50000000000000)) ||
        bits(ohms) != (b ? UINT64_C(0x4075e00000000000) : UINT64_C(0x405e000000000000)) ||
        bits(poisson) != (b ? UINT64_C(0x3fd8000000000000) : UINT64_C(0x3fd0000000000000)) ||
        bits(lead) != (b ? UINT64_C(0x3fe8000000000000) : UINT64_C(0x3fe0000000000000)))
        return bad("DAQmxCreateAIStrainGageChan", id, "frozen fifteen strain slots");
    return engineering_commit("DAQmxCreateAIStrainGageChan", id, name, b ? 0 : 10045);
}
BRIDGE_API int32 DAQmxCreateAIBridgeChan(TaskHandle handle, const char *name, const char *assigned,
        float64 minimum, float64 maximum, int32 units, int32 bridge, int32 source, float64 volts,
        float64 ohms, const char *scale) {
    uint64_t id = enter("DAQmxCreateAIBridgeChan");
    int32 setup = engineering_add("DAQmxCreateAIBridgeChan", id, handle, name, assigned, 11);
    if (setup) return setup;
    uInt32 i = engineering_mixed() ? 0 : owned->channels; unsigned b = i % 2;
    int32 expected_bridge = mode("All") ? (i == 0 ? 10182 : i == 1 ? 10187 : 10270) : b ? 10270 : 10182;
    int32 expected_source = mode("None") ? 10230 : b ? 10167 : 10200;
    uint64_t expected_volts = mode("None") ? 0 : b ? UINT64_C(0x4014000000000000) : UINT64_C(0x4004000000000000);
    trace("DAQmxCreateAIBridgeChan", "arguments", id,
        ",\"physical\":\"%s\",\"virtual_empty\":true,\"minimum_bits\":\"%016" PRIx64 "\",\"maximum_bits\":\"%016" PRIx64
        "\",\"units\":%" PRId32 ",\"bridge\":%" PRId32 ",\"excitation_source\":%" PRId32
        ",\"excitation_bits\":\"%016" PRIx64 "\",\"ohms_bits\":\"%016" PRIx64 "\",\"custom_scale_null\":%s",
        name,bits(minimum),bits(maximum),units,bridge,source,bits(volts),bits(ohms),scale ? "false":"true");
    if (units != (b ? 15897 : 15896) || bridge != expected_bridge || source != expected_source || bits(volts) != expected_volts || scale ||
        bits(minimum) != (b ? UINT64_C(0xc004000000000000) : UINT64_C(0xbfa0000000000000)) ||
        bits(maximum) != (b ? UINT64_C(0x400e000000000000) : UINT64_C(0x3f90000000000000)) ||
        bits(ohms) != (b ? UINT64_C(0x408f400000000000) : UINT64_C(0x4075e00000000000)))
        return bad("DAQmxCreateAIBridgeChan", id, "frozen eleven bridge slots");
    return engineering_commit("DAQmxCreateAIBridgeChan", id, name, b ? 10066 : 10050);
}
ACCEL_API int32 DAQmxCreateAIAccelChan(TaskHandle handle, const char *name, const char *assigned,
        int32 terminal, float64 minimum, float64 maximum, int32 units, float64 sensitivity,
        int32 sensitivity_units, int32 source, float64 amperes, const char *scale) {
    uint64_t id = enter("DAQmxCreateAIAccelChan");
    int32 setup = engineering_add("DAQmxCreateAIAccelChan", id, handle, name, assigned, 12);
    if (setup) return setup;
    uInt32 i = engineering_mixed() ? 0 : owned->channels;
    const int32 terminals[3]={12529,-1,10106}, units_expected[3]={10186,12470,12471};
    const uint64_t minima[3]={UINT64_C(0xc050000000000000),UINT64_C(0xc040000000000000),UINT64_C(0xc060000000000000)};
    const uint64_t maxima[3]={UINT64_C(0x4060000000000000),UINT64_C(0x4070000000000000),UINT64_C(0x4080000000000000)};
    const uint64_t sensitivities[3]={UINT64_C(0x405f400000000000),UINT64_C(0x3fc0000000000000),UINT64_C(0x406f400000000000)};
    int32 expected_source = mode("None") || i == 2 ? 10230 : i == 0 ? 10200 : 10167;
    uint64_t expected_amperes = mode("None") || i == 2 ? UINT64_C(0x3f50000000000000) : i == 0 ? UINT64_C(0x3f60000000000000) : UINT64_C(0x3f70000000000000);
    trace("DAQmxCreateAIAccelChan", "arguments", id,
        ",\"physical\":\"%s\",\"virtual_empty\":true,\"terminal\":%" PRId32 ",\"minimum_bits\":\"%016" PRIx64
        "\",\"maximum_bits\":\"%016" PRIx64 "\",\"units\":%" PRId32 ",\"sensitivity_bits\":\"%016" PRIx64
        "\",\"sensitivity_units\":%" PRId32 ",\"excitation_source\":%" PRId32 ",\"excitation_bits\":\"%016" PRIx64 "\",\"custom_scale_null\":%s",
        name,terminal,bits(minimum),bits(maximum),units,bits(sensitivity),sensitivity_units,source,bits(amperes),scale ? "false":"true");
    if (i >= 3 || terminal != terminals[i] || units != units_expected[i] || bits(minimum) != minima[i] || bits(maximum) != maxima[i] ||
        bits(sensitivity) != sensitivities[i] || sensitivity_units != (i == 1 ? 12510 : 12509) || source != expected_source || bits(amperes) != expected_amperes || scale)
        return bad("DAQmxCreateAIAccelChan", id, "frozen twelve acceleration slots");
    return engineering_commit("DAQmxCreateAIAccelChan", id, name, i == 0 ? 10045 : i == 1 ? 10050 : 10066);
}
COUPLING_API int32 DAQmxSetAICoupling(TaskHandle handle, const char *name, int32 value) {
    uint64_t id=enter("DAQmxSetAICoupling");
    if (!valid(handle) || owned->started || !name)
        return bad("DAQmxSetAICoupling", id, "selected coupling on existing channel before Start");
    uInt32 i=0;while(i<owned->channels&&strcmp(name,owned->channel[i]))++i;
    if(i==owned->channels||!owned->coupling_expected[i])return bad("DAQmxSetAICoupling",id,"explicit existing coupling channel");
    if (strcmp(name,owned->channel[i]) || owned->coupling_set[i] || value != owned->coupling_expected[i])
        return bad("DAQmxSetAICoupling", id, "exact requested coupling/channel, once");
    owned->coupling_set[i]=1;
    trace("DAQmxSetAICoupling","arguments",id,",\"channel\":\"%s\",\"coupling\":%" PRId32,name,value);
    return result("DAQmxSetAICoupling",id,mode("CouplingError") ? INT32_C(-990402) : 0);
}

#if defined(NIDAQMX_FIXTURE_FIVE_ONLY) || defined(NIDAQMX_FIXTURE_OMIT_RESISTANCE) || defined(NIDAQMX_FIXTURE_LEGACY_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_ONLY) || defined(NIDAQMX_FIXTURE_INPUT_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_CI_ONLY)
#define RESISTANCE_API __attribute__((visibility("hidden")))
#else
#define RESISTANCE_API API
#endif
#if defined(NIDAQMX_FIXTURE_FIVE_ONLY) || defined(NIDAQMX_FIXTURE_OMIT_THERM_IEX) || defined(NIDAQMX_FIXTURE_LEGACY_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_ONLY) || defined(NIDAQMX_FIXTURE_INPUT_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_CI_ONLY)
#define THERM_IEX_API __attribute__((visibility("hidden")))
#else
#define THERM_IEX_API API
#endif
#if defined(NIDAQMX_FIXTURE_FIVE_ONLY) || defined(NIDAQMX_FIXTURE_OMIT_THERM_VEX) || defined(NIDAQMX_FIXTURE_LEGACY_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_ONLY) || defined(NIDAQMX_FIXTURE_INPUT_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_CI_ONLY)
#define THERM_VEX_API __attribute__((visibility("hidden")))
#else
#define THERM_VEX_API API
#endif
#if defined(NIDAQMX_FIXTURE_FIVE_ONLY) || defined(NIDAQMX_FIXTURE_OMIT_FORCE) || defined(NIDAQMX_FIXTURE_LEGACY_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_ONLY) || defined(NIDAQMX_FIXTURE_INPUT_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_CI_ONLY)
#define FORCE_API __attribute__((visibility("hidden")))
#else
#define FORCE_API API
#endif
#if defined(NIDAQMX_FIXTURE_FIVE_ONLY) || defined(NIDAQMX_FIXTURE_OMIT_PRESSURE) || defined(NIDAQMX_FIXTURE_LEGACY_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_ONLY) || defined(NIDAQMX_FIXTURE_INPUT_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_CI_ONLY)
#define PRESSURE_API __attribute__((visibility("hidden")))
#else
#define PRESSURE_API API
#endif
#if defined(NIDAQMX_FIXTURE_FIVE_ONLY) || defined(NIDAQMX_FIXTURE_OMIT_TORQUE) || defined(NIDAQMX_FIXTURE_LEGACY_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_ONLY) || defined(NIDAQMX_FIXTURE_INPUT_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_CI_ONLY)
#define TORQUE_API __attribute__((visibility("hidden")))
#else
#define TORQUE_API API
#endif


/* Independently frozen1b53/ddea constructor controls. No local physical scaling. */
static int32 resistive_add(const char*fn,uint64_t id,TaskHandle h,const char*n,const char*a,int kind) {
    char device[MAX_NAME+1],suffix[32];
    if(!valid(h)||owned->timing||owned->started||owned->counted||!physical(n,device)||!empty(a))return bad(fn,id,"fresh individually owned resistive AI setup");
    if(!owned->channels)strcpy(scenario,device);
    if(strcmp(scenario,device))return bad(fn,id,"one explicit scenario device");
    uInt32 i=owned->channels;
    unsigned limit=five_mixed()?18U:resistive_mixed()?13U:mode("All")?(kind==18?3U:kind<=20?4U:kind<=22?3U:4U):2U;
    if(i>=limit||(resistive_mixed()?i<7||kind!=(int)i+11:owned->measurement&&owned->measurement!=kind))return bad(fn,id,"frozen ordered resistive/physical constructor union");
    snprintf(suffix,sizeof(suffix),"ai%" PRIu32,i);
    if(strcmp(strrchr(n,'/')+1,suffix))return bad(fn,id,"exact constructor column index");
    owned->measurement=resistive_mixed()?24:kind;return 0;
}
static int32 resistive_scalar(const char*fn,uint64_t id,TaskHandle h,const char*n,const char*a,int kind,
        float64 minimum,float64 maximum,int32 units,int32 wire,int32 source,float64 excitation,
        float64 ca,float64 cb,float64 cc,float64 reference,const char*scale) {
    int32 setup=resistive_add(fn,id,h,n,a,kind);if(setup)return setup;
    uInt32 i=resistive_mixed()?0:owned->channels;unsigned j=i%2;
    const uint64_t lo[3][2]={{0,UINT64_C(0x4040000000000000)},{UINT64_C(0xc040000000000000),UINT64_C(0xc050000000000000)},{UINT64_C(0xc030000000000000),0}};
    const uint64_t hi[3][2]={{UINT64_C(0x4080000000000000),UINT64_C(0x40a0000000000000)},{UINT64_C(0x4070000000000000),UINT64_C(0x4080000000000000)},{UINT64_C(0x4060000000000000),UINT64_C(0x4080000000000000)}};
    const uint64_t ex[3][2]={{UINT64_C(0x3f50000000000000),UINT64_C(0x3f58000000000000)},{UINT64_C(0x3f50000000000000),UINT64_C(0x3f58000000000000)},{UINT64_C(0x4004000000000000),UINT64_C(0x3ff4000000000000)}};
    const int32 wires[3][2]={{3,4},{3,2},{4,2}},sources[3][2]={{10200,10167},{10200,10167},{10167,10200}};
    const int32 temperatures[4]={10143,10144,10325,10145};
    int k=kind-18;int32 expected_units=kind==18?10384:mode("All")?temperatures[i]:kind==20&&j?10325:j?10144:10143;
    int32 expected_wire=wires[k][j];if(mode("All")){const int32 rw[3]={3,4,2},iw[4]={3,2,4,3},vw[4]={4,2,3,4};expected_wire=kind==18?rw[i]:kind==19?iw[i]:vw[i];}
    int32 expected_source=sources[k][j];uint64_t expected_ex=ex[k][j];
    if(mode("None")){expected_source=10230;expected_ex=kind==18?UINT64_C(0x3f40000000000000):kind==19?0:UINT64_C(0x8000000000000000);}
    uint64_t ea=j?UINT64_C(0x3f54000000000000):UINT64_C(0x3f52000000000000),eb=j?UINT64_C(0x3f28000000000000):UINT64_C(0x3f30000000000000),ec=j?UINT64_C(0x3e78000000000000):UINT64_C(0x3e80000000000000);
    if(mode("SignedCoefficients")){if(kind==19)ea=UINT64_C(0xbf52000000000000);else eb=UINT64_C(0xbf28000000000000);ec=UINT64_C(0x8000000000000000);}
    trace(fn,"arguments",id,",\"physical\":\"%s\",\"virtual_empty\":true,\"minimum_bits\":\"%016" PRIx64 "\",\"maximum_bits\":\"%016" PRIx64
        "\",\"units\":%" PRId32 ",\"resistance_config\":%" PRId32 ",\"excitation_source\":%" PRId32 ",\"excitation_bits\":\"%016" PRIx64
        "\",\"a_bits\":\"%016" PRIx64 "\",\"b_bits\":\"%016" PRIx64 "\",\"c_bits\":\"%016" PRIx64 "\",\"r1_bits\":\"%016" PRIx64 "\",\"custom_scale_null\":%s,\"scale_slot_present\":%s",
        n,bits(minimum),bits(maximum),units,wire,source,bits(excitation),bits(ca),bits(cb),bits(cc),bits(reference),scale?"false":"true",kind==18?"true":"false");
    if(bits(minimum)!=lo[k][j]||bits(maximum)!=hi[k][j]||units!=expected_units||wire!=expected_wire||source!=expected_source||bits(excitation)!=expected_ex||scale)
        return bad(fn,id,"frozen range/unit/wire/source/amount/scale slots");
    if(kind!=18&&(bits(ca)!=ea||bits(cb)!=eb||bits(cc)!=ec))return bad(fn,id,"literal A/B/C slots and signedzero, no curve/temperature conversion");
    if(kind==20&&bits(reference)!=(j?UINT64_C(0x40c7700000000000):UINT64_C(0x40c3880000000000)))return bad(fn,id,"Vex-only reference resistor in ohms");
    int32 coupling=kind==18?(j?0:10045):kind==19?(j?10066:10050):(j?10045:0);
    return engineering_commit(fn,id,n,coupling);
}
RESISTANCE_API int32 DAQmxCreateAIResistanceChan(TaskHandle h,const char*n,const char*a,float64 lo,float64 hi,int32 u,int32 w,int32 s,float64 e,const char*scale){uint64_t id=enter("DAQmxCreateAIResistanceChan");return resistive_scalar("DAQmxCreateAIResistanceChan",id,h,n,a,18,lo,hi,u,w,s,e,0,0,0,0,scale);}
THERM_IEX_API int32 DAQmxCreateAIThrmstrChanIex(TaskHandle h,const char*n,const char*a,float64 lo,float64 hi,int32 u,int32 w,int32 s,float64 e,float64 ca,float64 cb,float64 cc){uint64_t id=enter("DAQmxCreateAIThrmstrChanIex");return resistive_scalar("DAQmxCreateAIThrmstrChanIex",id,h,n,a,19,lo,hi,u,w,s,e,ca,cb,cc,0,NULL);}
THERM_VEX_API int32 DAQmxCreateAIThrmstrChanVex(TaskHandle h,const char*n,const char*a,float64 lo,float64 hi,int32 u,int32 w,int32 s,float64 e,float64 ca,float64 cb,float64 cc,float64 r){uint64_t id=enter("DAQmxCreateAIThrmstrChanVex");return resistive_scalar("DAQmxCreateAIThrmstrChanVex",id,h,n,a,20,lo,hi,u,w,s,e,ca,cb,cc,r,NULL);}
static int32 physical_bridge(const char*fn,uint64_t id,TaskHandle h,const char*n,const char*a,int kind,
        float64 lo,float64 hi,int32 units,int32 config,int32 source,float64 volts,float64 ohms,
        float64 e1,float64 e2,int32 eu,float64 p1,float64 p2,int32 pu,const char*scale){
    int32 setup=resistive_add(fn,id,h,n,a,kind);if(setup)return setup;
    uInt32 i=resistive_mixed()?0:owned->channels;unsigned j=i%2;int k=kind-21;
    const uint64_t data[3][2][8]={
        {{UINT64_C(0xc050000000000000),UINT64_C(0x4090000000000000),UINT64_C(0x4004000000000000),UINT64_C(0x4075e00000000000),UINT64_C(0xbf50000000000000),UINT64_C(0x3f78000000000000),UINT64_C(0xc010000000000000),UINT64_C(0x4060000000000000)},
         {UINT64_C(0xc060000000000000),UINT64_C(0x4080000000000000),UINT64_C(0x4014000000000000),UINT64_C(0x408f400000000000),UINT64_C(0xbf50000000000000),UINT64_C(0x3f78000000000000),UINT64_C(0x4050000000000000),UINT64_C(0xc040000000000000)}},
        {{UINT64_C(0xc100000000000000),UINT64_C(0x4120000000000000),UINT64_C(0x4014000000000000),UINT64_C(0x408f400000000000),UINT64_C(0xbf40000000000000),UINT64_C(0x3f70000000000000),UINT64_C(0xbfe0000000000000),UINT64_C(0x4008000000000000)},
         {UINT64_C(0xbfc0000000000000),UINT64_C(0x4050000000000000),UINT64_C(0x3ff4000000000000),UINT64_C(0x4075e00000000000),0,UINT64_C(0x3f60000000000000),UINT64_C(0x4030000000000000),UINT64_C(0x40a0000000000000)}},
        {{UINT64_C(0xc020000000000000),UINT64_C(0x4050000000000000),UINT64_C(0x3ff4000000000000),UINT64_C(0x405e000000000000),UINT64_C(0xbf30000000000000),UINT64_C(0x3f68000000000000),UINT64_C(0xc030000000000000),UINT64_C(0x4080000000000000)},
         {UINT64_C(0xc030000000000000),UINT64_C(0x4050000000000000),UINT64_C(0x4004000000000000),UINT64_C(0x4075e00000000000),UINT64_C(0xbf58000000000000),UINT64_C(0x3f50000000000000),UINT64_C(0x3ff8000000000000),UINT64_C(0xbffc000000000000)}}};
    uint64_t expected[8];memcpy(expected,data[k][j],sizeof(expected));
    const int32 output[3][2]={{15875,15877},{10081,15879},{15881,15884}},physical[3][2]={{15876,15875},{15880,10081},{15882,15881}},bridges[3][2]={{10182,10187},{10187,10182},{10270,10182}},sources[3][2]={{10200,10167},{10167,10200},{10200,10167}};
    int32 out=output[k][j],phys=physical[k][j],bridge=bridges[k][j],src=sources[k][j],electrical=j?15897:15896;
    if(k==1)electrical=j?15896:15897;
    if(mode("All")&&i>=2){out=k==0?15876:k==1?15880:i==2?15882:15883;phys=k==0?15877:k==1?15879:i==2?15883:15884;bridge=k==2&&i==2?10187:10270;}
    if(mode("None")){src=10230;expected[2]=k==0?UINT64_C(0x3ff4000000000000):k==1?0:UINT64_C(0x8000000000000000);}
    if(mode("SwapPairs")){uint64_t t=expected[4];expected[4]=expected[5];expected[5]=t;t=expected[6];expected[6]=expected[7];expected[7]=t;}
    if(mode("EqualPhysicalReject"))expected[7]=expected[6];
    trace(fn,"arguments",id,",\"physical\":\"%s\",\"virtual_empty\":true,\"minimum_bits\":\"%016" PRIx64 "\",\"maximum_bits\":\"%016" PRIx64
        "\",\"units\":%" PRId32 ",\"bridge\":%" PRId32 ",\"excitation_source\":%" PRId32 ",\"excitation_bits\":\"%016" PRIx64 "\",\"ohms_bits\":\"%016" PRIx64
        "\",\"first_electrical_bits\":\"%016" PRIx64 "\",\"second_electrical_bits\":\"%016" PRIx64 "\",\"electrical_units\":%" PRId32
        ",\"first_physical_bits\":\"%016" PRIx64 "\",\"second_physical_bits\":\"%016" PRIx64 "\",\"physical_units\":%" PRId32 ",\"custom_scale_null\":%s",
        n,bits(lo),bits(hi),units,config,source,bits(volts),bits(ohms),bits(e1),bits(e2),eu,bits(p1),bits(p2),pu,scale?"false":"true");
    uint64_t actual[8]={bits(lo),bits(hi),bits(volts),bits(ohms),bits(e1),bits(e2),bits(p1),bits(p2)};
    if(memcmp(actual,expected,sizeof(expected))||units!=out||config!=bridge||source!=src||eu!=electrical||pu!=phys||scale)return bad(fn,id,"frozen17slots/three separate unit roles/pair order/excitation");
    if(mode("EqualPhysicalReject"))return result(fn,id,INT32_C(-201334)); /* producer-controlled native rejection, not universal NI acceptance rule */
    int32 coupling=k==0?(j?0:10050):k==1?(j?10045:10066):(j?10050:0);
    return engineering_commit(fn,id,n,coupling);
}
FORCE_API int32 DAQmxCreateAIForceBridgeTwoPointLinChan(TaskHandle h,const char*n,const char*a,float64 lo,float64 hi,int32 u,int32 bc,int32 s,float64 v,float64 r,float64 e1,float64 e2,int32 eu,float64 p1,float64 p2,int32 pu,const char*scale){uint64_t id=enter("DAQmxCreateAIForceBridgeTwoPointLinChan");return physical_bridge("DAQmxCreateAIForceBridgeTwoPointLinChan",id,h,n,a,21,lo,hi,u,bc,s,v,r,e1,e2,eu,p1,p2,pu,scale);}
PRESSURE_API int32 DAQmxCreateAIPressureBridgeTwoPointLinChan(TaskHandle h,const char*n,const char*a,float64 lo,float64 hi,int32 u,int32 bc,int32 s,float64 v,float64 r,float64 e1,float64 e2,int32 eu,float64 p1,float64 p2,int32 pu,const char*scale){uint64_t id=enter("DAQmxCreateAIPressureBridgeTwoPointLinChan");return physical_bridge("DAQmxCreateAIPressureBridgeTwoPointLinChan",id,h,n,a,22,lo,hi,u,bc,s,v,r,e1,e2,eu,p1,p2,pu,scale);}
TORQUE_API int32 DAQmxCreateAITorqueBridgeTwoPointLinChan(TaskHandle h,const char*n,const char*a,float64 lo,float64 hi,int32 u,int32 bc,int32 s,float64 v,float64 r,float64 e1,float64 e2,int32 eu,float64 p1,float64 p2,int32 pu,const char*scale){uint64_t id=enter("DAQmxCreateAITorqueBridgeTwoPointLinChan");return physical_bridge("DAQmxCreateAITorqueBridgeTwoPointLinChan",id,h,n,a,23,lo,hi,u,bc,s,v,r,e1,e2,eu,p1,p2,pu,scale);}

/* Fixed12/12/10/13/13 constructor slots; no header or native implementation imports. */
#if defined(NIDAQMX_FIXTURE_OMIT_FIVE) || defined(NIDAQMX_FIXTURE_OMIT_VELOCITY_IEPE) || defined(NIDAQMX_FIXTURE_LEGACY_ONLY) || defined(NIDAQMX_FIXTURE_RESISTIVE_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_ONLY) || defined(NIDAQMX_FIXTURE_INPUT_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_CI_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_AI_ONLY) || defined(NIDAQMX_FIXTURE_AI_ONLY) || defined(NIDAQMX_FIXTURE_DIGITAL_ONLY) || defined(NIDAQMX_FIXTURE_COUNT_ONLY) || defined(NIDAQMX_FIXTURE_FREQUENCY_ONLY)
#define VELOCITY_IEPE_API __attribute__((visibility("hidden")))
#else
#define VELOCITY_IEPE_API API
#endif
#if defined(NIDAQMX_FIXTURE_OMIT_FIVE) || defined(NIDAQMX_FIXTURE_OMIT_FORCE_IEPE) || defined(NIDAQMX_FIXTURE_LEGACY_ONLY) || defined(NIDAQMX_FIXTURE_RESISTIVE_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_ONLY) || defined(NIDAQMX_FIXTURE_INPUT_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_CI_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_AI_ONLY) || defined(NIDAQMX_FIXTURE_AI_ONLY) || defined(NIDAQMX_FIXTURE_DIGITAL_ONLY) || defined(NIDAQMX_FIXTURE_COUNT_ONLY) || defined(NIDAQMX_FIXTURE_FREQUENCY_ONLY)
#define FORCE_IEPE_API __attribute__((visibility("hidden")))
#else
#define FORCE_IEPE_API API
#endif
#if defined(NIDAQMX_FIXTURE_OMIT_FIVE) || defined(NIDAQMX_FIXTURE_OMIT_CHARGE_ACCEL) || defined(NIDAQMX_FIXTURE_LEGACY_ONLY) || defined(NIDAQMX_FIXTURE_RESISTIVE_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_ONLY) || defined(NIDAQMX_FIXTURE_INPUT_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_CI_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_AI_ONLY) || defined(NIDAQMX_FIXTURE_AI_ONLY) || defined(NIDAQMX_FIXTURE_DIGITAL_ONLY) || defined(NIDAQMX_FIXTURE_COUNT_ONLY) || defined(NIDAQMX_FIXTURE_FREQUENCY_ONLY)
#define CHARGE_ACCEL_API __attribute__((visibility("hidden")))
#else
#define CHARGE_ACCEL_API API
#endif
#if defined(NIDAQMX_FIXTURE_OMIT_FIVE) || defined(NIDAQMX_FIXTURE_OMIT_LVDT) || defined(NIDAQMX_FIXTURE_LEGACY_ONLY) || defined(NIDAQMX_FIXTURE_RESISTIVE_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_ONLY) || defined(NIDAQMX_FIXTURE_INPUT_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_CI_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_AI_ONLY) || defined(NIDAQMX_FIXTURE_AI_ONLY) || defined(NIDAQMX_FIXTURE_DIGITAL_ONLY) || defined(NIDAQMX_FIXTURE_COUNT_ONLY) || defined(NIDAQMX_FIXTURE_FREQUENCY_ONLY)
#define LVDT_API __attribute__((visibility("hidden")))
#else
#define LVDT_API API
#endif
#if defined(NIDAQMX_FIXTURE_OMIT_FIVE) || defined(NIDAQMX_FIXTURE_OMIT_RVDT) || defined(NIDAQMX_FIXTURE_LEGACY_ONLY) || defined(NIDAQMX_FIXTURE_RESISTIVE_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_ONLY) || defined(NIDAQMX_FIXTURE_INPUT_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_CI_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_AI_ONLY) || defined(NIDAQMX_FIXTURE_AI_ONLY) || defined(NIDAQMX_FIXTURE_DIGITAL_ONLY) || defined(NIDAQMX_FIXTURE_COUNT_ONLY) || defined(NIDAQMX_FIXTURE_FREQUENCY_ONLY)
#define RVDT_API __attribute__((visibility("hidden")))
#else
#define RVDT_API API
#endif
static int32 five_add(const char *fn,uint64_t id,TaskHandle h,const char *name,const char *assigned,int kind) {
    char device[MAX_NAME+1],suffix[32];
    if(!valid(h)||owned->timing||owned->started||owned->counted||owned->tc_pending_setter||!physical(name,device)||!empty(assigned))return bad(fn,id,"fresh selected individual NI-five constructor");
    if(!owned->channels)strcpy(scenario,device);
    if(strcmp(scenario,device))return bad(fn,id,"one explicit NI-five scenario device");
    uInt32 i=owned->channels;unsigned limit=five_mixed()?18U:mode("All")?3U:2U;
    if(i>=limit||(five_mixed()?i<13||kind!=(int)i+12:owned->measurement&&owned->measurement!=kind))return bad(fn,id,"exact ordered five constructor union");
    snprintf(suffix,sizeof(suffix),"ai%" PRIu32,i);if(strcmp(strrchr(name,'/')+1,suffix))return bad(fn,id,"explicit ordinal matches physical AI");
    owned->measurement=five_mixed()?30:kind;return 0;
}
static int32 five_create(const char *fn,uint64_t id,TaskHandle h,const char *name,const char *assigned,int kind,
        int32 terminal,float64 lo,float64 hi,int32 units,float64 sensitivity,int32 sens_units,int32 source,float64 excitation,float64 hz,int32 wire,const char *scale) {
    int32 setup=five_add(fn,id,h,name,assigned,kind);if(setup)return setup;
    unsigned k=(unsigned)(kind-25),j=five_mixed()?0:owned->channels%2;
    const uint64_t expected_words[5][2][5]={
        {{UINT64_C(0xbfd8000000000000),UINT64_C(0x3ffa000000000000),UINT64_C(0x3fd4000000000000),UINT64_C(0x3f60000000000000),UINT64_C(0x0000000000000000)},
         {UINT64_C(0xbfdc000000000000),UINT64_C(0x3ff5000000000000),UINT64_C(0x3fd2000000000000),UINT64_C(0x3f68000000000000),UINT64_C(0x0000000000000000)}},
        {{UINT64_C(0xc004000000000000),UINT64_C(0x4023000000000000),UINT64_C(0x3fec000000000000),UINT64_C(0x3f64000000000000),UINT64_C(0x0000000000000000)},
         {UINT64_C(0xbffc000000000000),UINT64_C(0x4026800000000000),UINT64_C(0x3fe2000000000000),UINT64_C(0x3f5c000000000000),UINT64_C(0x0000000000000000)}},
        {{UINT64_C(0xbfe2000000000000),UINT64_C(0x4010400000000000),UINT64_C(0x3fd6000000000000),UINT64_C(0x0000000000000000),UINT64_C(0x0000000000000000)},
         {UINT64_C(0xbff6000000000000),UINT64_C(0x4022400000000000),UINT64_C(0x3fca000000000000),UINT64_C(0x0000000000000000),UINT64_C(0x0000000000000000)}},
        {{UINT64_C(0xbfda000000000000),UINT64_C(0x3feb000000000000),UINT64_C(0x3fce000000000000),UINT64_C(0x4001000000000000),UINT64_C(0x4074080000000000)},
         {UINT64_C(0xbfce000000000000),UINT64_C(0x3fec800000000000),UINT64_C(0x3fc3000000000000),UINT64_C(0x4005000000000000),UINT64_C(0x4074040000000000)}},
        {{UINT64_C(0xbff1000000000000),UINT64_C(0x4016c00000000000),UINT64_C(0x3fc7000000000000),UINT64_C(0x4009000000000000),UINT64_C(0x4074020000000000)},
         {UINT64_C(0xbfe3000000000000),UINT64_C(0x4009400000000000),UINT64_C(0x3fbb000000000000),UINT64_C(0x400d000000000000),UINT64_C(0x4074010000000000)}},
    };
    const int32 expected_ints[5][2][5]={
        {{10106,15959,15963,10200,0}, {10083,15960,15964,10167,0}},
        {{10078,15875,15891,10200,0}, {-1,15876,15892,10167,0}},
        {{10106,10186,16099,0,0}, {12529,12470,16101,0,0}},
        {{0,10219,12506,10200,4}, {0,10379,12505,10167,6}},
        {{0,10146,12508,10200,5}, {0,10273,12507,10230,4}},
    };
    uint64_t e[5];int32 v[5];memcpy(e,expected_words[k][j],sizeof(e));memcpy(v,expected_ints[k][j],sizeof(v));
    const int32 coupling[5][2]={{10045,0},{0,10050},{10066,0},{10050,0},{0,10045}};int32 cp=coupling[k][j];
    if(mode("All")){
        unsigned i=owned->channels;const uint64_t sens[3]={UINT64_C(0xbfd4000000000000),0,UINT64_C(0x8000000000000000)};const uint64_t amount[3]={UINT64_C(0x3f40000000000000),0,UINT64_C(0x8000000000000000)};const int32 cps[3]={10045,10050,10066};
        memcpy(e,expected_words[k][0],sizeof(e));memcpy(v,expected_ints[k][0],sizeof(v));e[2]=sens[i];cp=cps[i];
        if(k!=2){v[3]=10230;e[3]=amount[i];}
        if(k>=3){e[4]=UINT64_C(0x3fc0000000000000);v[4]=(int32)i+4;}
        if(k==1){const int32 u[3]={15877,15876,15875};v[1]=u[i];}
        if(k==2){const int32 u[3]={12471,10186,12470},su[3]={16100,16099,16101};v[1]=u[i];v[2]=su[i];}
    }
    trace(fn,"arguments",id,",\"physical\":\"%s\",\"virtual_empty\":true,\"terminal\":%" PRId32 ",\"minimum_bits\":\"%016" PRIx64 "\",\"maximum_bits\":\"%016" PRIx64 "\",\"units\":%" PRId32 ",\"sensitivity_bits\":\"%016" PRIx64 "\",\"sensitivity_units\":%" PRId32 ",\"excitation_source\":%" PRId32 ",\"excitation_bits\":\"%016" PRIx64 "\",\"frequency_bits\":\"%016" PRIx64 "\",\"ac_wire_mode\":%" PRId32 ",\"custom_scale_null\":%s,\"terminal_slot_present\":%s,\"excitation_slot_present\":%s",
        name,terminal,bits(lo),bits(hi),units,bits(sensitivity),sens_units,source,bits(excitation),bits(hz),wire,scale?"false":"true",k<3?"true":"false",k==2?"false":"true");
    uint64_t actual[5]={bits(lo),bits(hi),bits(sensitivity),bits(excitation),bits(hz)};int32 actual_ints[5]={terminal,units,sens_units,source,wire};
    if(memcmp(actual,e,sizeof(e))||memcmp(actual_ints,v,sizeof(v))||scale)return bad(fn,id,"frozen NI-five exact scalar/enum/pointer slots");
    if(mode("LaterFail")&&owned->channels==(five_mixed()?17U:1U))return result(fn,id,INT32_C(-990104));
    return engineering_commit(fn,id,name,cp);
}
VELOCITY_IEPE_API int32 DAQmxCreateAIVelocityIEPEChan(TaskHandle h,const char*n,const char*a,int32 t,float64 lo,float64 hi,int32 u,float64 se,int32 su,int32 src,float64 e,const char*sc){uint64_t id=enter("DAQmxCreateAIVelocityIEPEChan");return five_create("DAQmxCreateAIVelocityIEPEChan",id,h,n,a,25,t,lo,hi,u,se,su,src,e,0.0,0,sc);}
FORCE_IEPE_API int32 DAQmxCreateAIForceIEPEChan(TaskHandle h,const char*n,const char*a,int32 t,float64 lo,float64 hi,int32 u,float64 se,int32 su,int32 src,float64 e,const char*sc){uint64_t id=enter("DAQmxCreateAIForceIEPEChan");return five_create("DAQmxCreateAIForceIEPEChan",id,h,n,a,26,t,lo,hi,u,se,su,src,e,0.0,0,sc);}
CHARGE_ACCEL_API int32 DAQmxCreateAIAccelChargeChan(TaskHandle h,const char*n,const char*a,int32 t,float64 lo,float64 hi,int32 u,float64 se,int32 su,const char*sc){uint64_t id=enter("DAQmxCreateAIAccelChargeChan");return five_create("DAQmxCreateAIAccelChargeChan",id,h,n,a,27,t,lo,hi,u,se,su,0,0.0,0.0,0,sc);}
LVDT_API int32 DAQmxCreateAIPosLVDTChan(TaskHandle h,const char*n,const char*a,float64 lo,float64 hi,int32 u,float64 se,int32 su,int32 src,float64 e,float64 hz,int32 w,const char*sc){uint64_t id=enter("DAQmxCreateAIPosLVDTChan");return five_create("DAQmxCreateAIPosLVDTChan",id,h,n,a,28,0,lo,hi,u,se,su,src,e,hz,w,sc);}
RVDT_API int32 DAQmxCreateAIPosRVDTChan(TaskHandle h,const char*n,const char*a,float64 lo,float64 hi,int32 u,float64 se,int32 su,int32 src,float64 e,float64 hz,int32 w,const char*sc){uint64_t id=enter("DAQmxCreateAIPosRVDTChan");return five_create("DAQmxCreateAIPosRVDTChan",id,h,n,a,29,0,lo,hi,u,se,su,src,e,hz,w,sc);}

API int32 DAQmxCreateTask(const char *name, TaskHandle *output) {
    uint64_t id = enter("DAQmxCreateTask");
    trace("DAQmxCreateTask", "arguments", id, ",\"empty_name\":%s,\"output_present\":%s",
          empty(name) ? "true" : "false", output ? "true" : "false");
    if (!empty(name) || !output || *output != NULL || created || calls > MAX_CALLS)
        return bad("DAQmxCreateTask", id, "one empty newly owned task required");
    ++created;
#ifdef NIDAQMX_FIXTURE_CREATE_FAIL_NONNULL
    strcpy(scenario, "CreateFailNonNull");
    *output = (TaskHandle)(uintptr_t)UINT32_C(0x1a2b3c4d);
    trace("DAQmxCreateTask", "output", id, ",\"output_nonnull\":true,\"qualified_owned\":false");
    return result("DAQmxCreateTask", id, INT32_C(-50103));
#else
    owned = calloc(1, sizeof(*owned));
    if (!owned) return result("DAQmxCreateTask", id, INT32_C(-990101));
    *output = (TaskHandle)owned;
    trace("DAQmxCreateTask", "output", id, ",\"output_nonnull\":true,\"qualified_owned\":true");
#ifdef NIDAQMX_FIXTURE_CREATE_WARNING
    return result("DAQmxCreateTask", id, INT32_C(12345));
#else
    return result("DAQmxCreateTask", id, 0);
#endif
#endif
}

#ifndef NIDAQMX_FIXTURE_OMIT_VOLTAGE
OLD_AI_API int32 DAQmxCreateAIVoltageChan(TaskHandle handle, const char *name,
        const char *virtual_name, int32 terminal, float64 minimum, float64 maximum,
        int32 units, const char *custom_scale) {
    uint64_t id = enter("DAQmxCreateAIVoltageChan");
    if (mixed_physical(name)) {
        MixedArgs a = {.kind=1,.terminal=terminal,.units=units,.minimum=minimum,.maximum=maximum,.null_scale=custom_scale==NULL};
        return mixed_add("DAQmxCreateAIVoltageChan", id, handle, name, virtual_name, &a);
    }
    char device[MAX_NAME + 1];
    if (!valid(handle) || owned->timing || (owned->measurement != 0 && owned->measurement != 1) || !physical(name, device) ||
        !empty(virtual_name) || custom_scale != NULL ||
        !(terminal == -1 || terminal == 10083 || terminal == 10078 || terminal == 10106 || terminal == 12529) ||
        !isfinite(minimum) || !isfinite(maximum) || !(minimum < maximum) || units != 10348 ||
        owned->channels >= MAX_CHANNELS)
        return bad("DAQmxCreateAIVoltageChan", id, "individual AI voltage parameters/order");
    if (owned->channels == 0) strcpy(scenario, device);
    else if (strcmp(scenario, device) != 0) return bad("DAQmxCreateAIVoltageChan", id, "mixed fixture scenario devices");
    owned->measurement = 1;
    trace("DAQmxCreateAIVoltageChan", "arguments", id,
        ",\"physical\":\"%s\",\"virtual_empty\":true,\"terminal\":%" PRId32
        ",\"minimum_bits\":\"%016" PRIx64 "\",\"maximum_bits\":\"%016" PRIx64
        "\",\"units\":%" PRId32 ",\"custom_scale_null\":true",
        name, terminal, bits(minimum), bits(maximum), units);
    if (mode("Busy")) return result("DAQmxCreateAIVoltageChan", id, INT32_C(-50103));
    for (uInt32 i = 0; i < owned->channels; ++i)
        if (strcmp(name, owned->channel[i]) == 0) return bad("DAQmxCreateAIVoltageChan", id, "duplicate configured name");
    strcpy(owned->channel[owned->channels++], name);
    return result("DAQmxCreateAIVoltageChan", id, mode("SetupWarn") && owned->channels == 1 ? 12345 : 0);
}
#endif
#ifndef NIDAQMX_FIXTURE_OMIT_TC
OLD_AI_API int32 DAQmxCreateAIThrmcplChan(TaskHandle handle, const char *name,
        const char *virtual_name, float64 minimum, float64 maximum, int32 units,
        int32 thermocouple_type, int32 cjc_source, float64 cjc_value, const char *cjc_channel) {
    uint64_t id = enter("DAQmxCreateAIThrmcplChan");
    if (mixed_physical(name)) {
        MixedArgs a = {.kind=2,.units=units,.type=thermocouple_type,.source=cjc_source,.minimum=minimum,.maximum=maximum,.a=cjc_value,.empty_aux=empty(cjc_channel)};
        return mixed_add("DAQmxCreateAIThrmcplChan", id, handle, name, virtual_name, &a);
    }
    char device[MAX_NAME + 1], suffix[32];
    if (!valid(handle) || owned->timing || (owned->measurement != 0 && owned->measurement != 2) || owned->tc_pending_setter ||
        !physical(name, device) || !empty(virtual_name) || !empty(cjc_channel) ||
        !isfinite(minimum) || !isfinite(maximum) || !(minimum < maximum) ||
        !isfinite(cjc_value) || owned->channels >= MAX_CHANNELS)
        return bad("DAQmxCreateAIThrmcplChan", id, "individual TC constructor parameters and per-channel setter order");
    if (owned->channels == 0) strcpy(scenario, device);
    else if (strcmp(scenario, device) != 0) return bad("DAQmxCreateAIThrmcplChan", id, "mixed fixture scenario devices");
    owned->measurement = 2;
    uInt32 index = owned->channels;
    snprintf(suffix, sizeof(suffix), "ai%" PRIu32, index);
    if (strcmp(strrchr(name, '/') + 1, suffix) != 0)
        return bad("DAQmxCreateAIThrmcplChan", id, "explicit ordered individual TC physical names");
    int32 expected_units, expected_type, expected_cjc;
    uint64_t expected_max, expected_cjc_bits;
    if (mode("TCAll")) {
        static const int32 type_codes[10] = {10072,10073,10077,10082,10085,10086,10047,10055,16208,16209};
        static const int32 unit_codes[4] = {10143,10144,10325,10145};
        if (index >= 10) return bad("DAQmxCreateAIThrmcplChan", id, "finite ten-type oracle");
        expected_type = type_codes[index]; expected_units = unit_codes[index % 4];
        expected_cjc = 10200; expected_cjc_bits = UINT64_C(0); expected_max = UINT64_C(0x407f400000000000);
    } else {
        if (index >= 2) return bad("DAQmxCreateAIThrmcplChan", id, "finite two-channel oracle");
        expected_type = index == 0 ? 10073 : 10086;
        expected_units = index == 0 ? 10143 : 10144;
        expected_cjc = index == 0 ? 10200 : 10116;
        expected_cjc_bits = index == 0 ? UINT64_C(0) : UINT64_C(0x4053400000000000);
        expected_max = index == 0 ? UINT64_C(0x407f400000000000) : UINT64_C(0x4072c00000000000);
    }
    trace("DAQmxCreateAIThrmcplChan", "arguments", id,
        ",\"physical\":\"%s\",\"virtual_empty\":true,\"minimum_bits\":\"%016" PRIx64
        "\",\"maximum_bits\":\"%016" PRIx64 "\",\"units\":%" PRId32
        ",\"thermocouple_type\":%" PRId32 ",\"cjc_source\":%" PRId32
        ",\"cjc_value_bits\":\"%016" PRIx64 "\",\"cjc_channel_empty\":true",
        name, bits(minimum), bits(maximum), units, thermocouple_type, cjc_source, bits(cjc_value));
    if (bits(minimum) != UINT64_C(0xc049000000000000) || bits(maximum) != expected_max ||
        units != expected_units || thermocouple_type != expected_type || cjc_source != expected_cjc ||
        bits(cjc_value) != expected_cjc_bits)
        return bad("DAQmxCreateAIThrmcplChan", id, "frozen exact TC range/type/unit/CJC arguments");
    if (mode("Busy")) return result("DAQmxCreateAIThrmcplChan", id, INT32_C(-50103));
    strcpy(owned->channel[owned->channels++], name);
    if ((mode("TCExplicit") || mode("TCTermError")) && index == 1) owned->tc_pending_setter = owned->channels;
    return result("DAQmxCreateAIThrmcplChan", id, mode("SetupWarn") && owned->channels == 1 ? 12345 : 0);
}
#endif
#ifndef NIDAQMX_FIXTURE_OMIT_TC_TERMINAL
OLD_AI_API int32 DAQmxSetAITermCfg(TaskHandle handle, const char *name, int32 value) {
    uint64_t id = enter("DAQmxSetAITermCfg");
    if (!valid(handle) || (owned->measurement != 2 && owned->measurement != 5) || owned->timing || !name ||
        !owned->tc_pending_setter || owned->tc_pending_setter != owned->channels ||
        strcmp(name, owned->channel[owned->tc_pending_setter - 1]) != 0 || value != 10106)
        return bad("DAQmxSetAITermCfg", id, "exact explicit TC channel immediately after its constructor");
    trace("DAQmxSetAITermCfg", "arguments", id, ",\"physical\":\"%s\",\"terminal\":%" PRId32, name, value);
    owned->tc_pending_setter = 0;
    return result("DAQmxSetAITermCfg", id, mode("TCTermError") ? INT32_C(-990203) : 0);
}
#endif
#ifndef NIDAQMX_FIXTURE_OMIT_CURRENT
OLD_AI_API int32 DAQmxCreateAICurrentChan(TaskHandle handle, const char *name,
        const char *virtual_name, int32 terminal, float64 minimum, float64 maximum,
        int32 units, int32 shunt_location, float64 external_ohms, const char *custom_scale) {
    uint64_t id = enter("DAQmxCreateAICurrentChan");
    if (mixed_physical(name)) {
        MixedArgs a = {.kind=3,.terminal=terminal,.units=units,.source=shunt_location,.minimum=minimum,.maximum=maximum,.a=external_ohms,.null_scale=custom_scale==NULL};
        return mixed_add("DAQmxCreateAICurrentChan", id, handle, name, virtual_name, &a);
    }
    char device[MAX_NAME + 1], suffix[32];
    if (!valid(handle) || owned->timing || (owned->measurement != 0 && owned->measurement != 3) ||
        owned->tc_pending_setter || !physical(name, device) || !empty(virtual_name) || custom_scale != NULL ||
        !isfinite(minimum) || !isfinite(maximum) || !(minimum < maximum) ||
        !isfinite(external_ohms) || !(external_ohms > 0) || units != 10342 || owned->channels >= 2)
        return bad("DAQmxCreateAICurrentChan", id, "explicit current channel, amps and NULL custom scale");
    if (owned->channels == 0) strcpy(scenario, device);
    else if (strcmp(scenario, device) != 0) return bad("DAQmxCreateAICurrentChan", id, "mixed fixture scenario devices");
    owned->measurement = 3;
    uInt32 index = owned->channels;
    snprintf(suffix, sizeof(suffix), "ai%" PRIu32, index);
    if (strcmp(strrchr(name, '/') + 1, suffix) != 0)
        return bad("DAQmxCreateAICurrentChan", id, "explicit ordered current physical names");
    int32 expected_terminal = index == 0 ? (mode("CurrentDefault") ? -1 : 10106) : 10083;
    int32 expected_location = index == 0 ? 10200 : 10167;
    uint64_t expected_minimum = index == 0 ? UINT64_C(0) : UINT64_C(0xbf947ae147ae147b);
    uint64_t expected_ohms = index == 0 ? UINT64_C(0x406f200000000000) : UINT64_C(0x406f400000000000);
    trace("DAQmxCreateAICurrentChan", "arguments", id,
        ",\"physical\":\"%s\",\"virtual_empty\":true,\"terminal\":%" PRId32
        ",\"minimum_bits\":\"%016" PRIx64 "\",\"maximum_bits\":\"%016" PRIx64
        "\",\"units\":%" PRId32 ",\"shunt_location\":%" PRId32
        ",\"external_ohms_argument_bits\":\"%016" PRIx64 "\",\"custom_scale_null\":true",
        name, terminal, bits(minimum), bits(maximum), units, shunt_location, bits(external_ohms));
    if (terminal != expected_terminal || bits(minimum) != expected_minimum ||
        bits(maximum) != UINT64_C(0x3f947ae147ae147b) || shunt_location != expected_location ||
        bits(external_ohms) != expected_ohms)
        return bad("DAQmxCreateAICurrentChan", id, "frozen current terminal/range/shunt ABI inputs");
    /* 249 is only the reference-wrapper ABI convention, not a measured shunt. */
    if (mode("Busy")) return result("DAQmxCreateAICurrentChan", id, INT32_C(-50103));
    strcpy(owned->channel[owned->channels++], name);
    return result("DAQmxCreateAICurrentChan", id, mode("SetupWarn") && owned->channels == 1 ? 12345 : 0);
}
#endif
#ifndef NIDAQMX_FIXTURE_OMIT_RTD
OLD_AI_API int32 DAQmxCreateAIRTDChan(TaskHandle handle, const char *name,
        const char *virtual_name, float64 minimum, float64 maximum, int32 units,
        int32 rtd_type, int32 resistance_config, int32 excitation_source,
        float64 excitation_amperes, float64 r0_ohms) {
    uint64_t id = enter("DAQmxCreateAIRTDChan");
    if (mixed_physical(name)) {
        MixedArgs a = {.kind=4,.units=units,.type=rtd_type,.source=excitation_source,.wire=resistance_config,.minimum=minimum,.maximum=maximum,.a=excitation_amperes,.b=r0_ohms};
        return mixed_add("DAQmxCreateAIRTDChan", id, handle, name, virtual_name, &a);
    }
    char device[MAX_NAME + 1], suffix[32];
    if (!valid(handle) || owned->timing || owned->tc_pending_setter ||
        (owned->measurement != 0 && owned->measurement != 4) ||
        !physical(name, device) || !empty(virtual_name) ||
        !isfinite(minimum) || !isfinite(maximum) || !(minimum < maximum) ||
        !isfinite(excitation_amperes) || !(excitation_amperes > 0) ||
        !isfinite(r0_ohms) || !(r0_ohms > 0) || owned->channels >= 6)
        return bad("DAQmxCreateAIRTDChan", id, "explicit finite eleven-slot RTD constructor");
    if (owned->channels == 0) strcpy(scenario, device);
    else if (strcmp(scenario, device) != 0) return bad("DAQmxCreateAIRTDChan", id, "mixed fixture scenario devices");
    owned->measurement = 4;
    uInt32 index = owned->channels;
    snprintf(suffix, sizeof(suffix), "ai%" PRIu32, index);
    if (strcmp(strrchr(name, '/') + 1, suffix) != 0)
        return bad("DAQmxCreateAIRTDChan", id, "explicit ordered RTD physical names");
    int32 expected_units, expected_type, expected_wire, expected_source;
    uint64_t expected_minimum, expected_maximum, expected_current, expected_r0;
    if (mode("RtdAll")) {
        static const int32 type_codes[6] = {12481,10071,12482,10069,10053,12483};
        static const int32 unit_codes[4] = {10143,10144,10325,10145};
        expected_type = type_codes[index]; expected_units = unit_codes[index % 4];
        expected_wire = (int32)(index % 3) + 2;
        expected_source = index % 2 == 0 ? 10200 : 10167;
        expected_minimum = UINT64_C(0x4040000000000000);
        expected_maximum = UINT64_C(0x4070000000000000);
        expected_current = index % 2 == 0 ? UINT64_C(0x3f50000000000000) : UINT64_C(0x3f60000000000000);
        expected_r0 = index % 2 == 0 ? UINT64_C(0x4059000000000000) : UINT64_C(0x408f400000000000);
    } else {
        if (index >= 2) return bad("DAQmxCreateAIRTDChan", id, "frozen asymmetric two-channel RTD oracle");
        expected_type = index == 0 ? 10071 : 12483;
        expected_units = index == 0 ? 10143 : 10144;
        expected_wire = index == 0 ? 3 : 4;
        expected_source = index == 0 ? 10200 : 10167;
        expected_minimum = index == 0 ? UINT64_C(0xc049000000000000) : UINT64_C(0xc044000000000000);
        expected_maximum = index == 0 ? UINT64_C(0x407f400000000000) : UINT64_C(0x4072c00000000000);
        expected_current = index == 0 ? UINT64_C(0x3f50000000000000) : UINT64_C(0x3f60000000000000);
        expected_r0 = index == 0 ? UINT64_C(0x4059000000000000) : UINT64_C(0x408f400000000000);
    }
    trace("DAQmxCreateAIRTDChan", "arguments", id,
        ",\"physical\":\"%s\",\"virtual_empty\":true,\"minimum_bits\":\"%016" PRIx64
        "\",\"maximum_bits\":\"%016" PRIx64 "\",\"units\":%" PRId32
        ",\"rtd_type\":%" PRId32 ",\"resistance_config\":%" PRId32
        ",\"excitation_source\":%" PRId32 ",\"excitation_amperes_bits\":\"%016" PRIx64
        "\",\"r0_ohms_bits\":\"%016" PRIx64 "\"",
        name, bits(minimum), bits(maximum), units, rtd_type, resistance_config,
        excitation_source, bits(excitation_amperes), bits(r0_ohms));
    if (bits(minimum) != expected_minimum || bits(maximum) != expected_maximum ||
        units != expected_units || rtd_type != expected_type || resistance_config != expected_wire ||
        excitation_source != expected_source || bits(excitation_amperes) != expected_current ||
        bits(r0_ohms) != expected_r0)
        return bad("DAQmxCreateAIRTDChan", id, "frozen exact RTD units/type/wire/excitation/R0 slots");
    if (mode("Busy")) return result("DAQmxCreateAIRTDChan", id, INT32_C(-50103));
    strcpy(owned->channel[owned->channels++], name);
    return result("DAQmxCreateAIRTDChan", id, mode("SetupWarn") && owned->channels == 1 ? 12345 : 0);
}
#endif
#ifndef NIDAQMX_FIXTURE_ON_DEMAND
API int32 DAQmxCfgSampClkTiming(TaskHandle handle, const char *source, float64 rate,
        int32 edge, int32 sample_mode, uInt64 buffer_hint) {
    uint64_t id = enter("DAQmxCfgSampClkTiming");
    if (!valid(handle) || owned->channels == 0 || owned->timing || owned->tc_pending_setter || !empty(source) ||
        !isfinite(rate) || !(rate > 0) || edge != 10280 || sample_mode != 10123 || buffer_hint == 0)
        return bad("DAQmxCfgSampClkTiming", id, "internal rising continuous timing required");
    trace("DAQmxCfgSampClkTiming", "arguments", id,
        ",\"source_empty\":true,\"rate_bits\":\"%016" PRIx64 "\",\"edge\":%" PRId32
        ",\"mode\":%" PRId32 ",\"buffer_hint_u64\":%" PRIu64, bits(rate), edge, sample_mode, buffer_hint);
    owned->timing = 1; owned->rate = rate; owned->hint = buffer_hint;
    return result("DAQmxCfgSampClkTiming", id, mode("SetupError") ? INT32_C(-990102) : 0);
}
API int32 DAQmxCfgInputBuffer(TaskHandle handle, uInt32 samples_per_channel) {
    uint64_t id = enter("DAQmxCfgInputBuffer");
    if (!valid(handle) || !owned->timing || owned->input_buffer || samples_per_channel == 0 ||
        owned->hint != (uInt64)samples_per_channel)
        return bad("DAQmxCfgInputBuffer", id, "one explicit per-channel host buffer matching timing hint");
    owned->input_buffer = samples_per_channel;
    trace("DAQmxCfgInputBuffer", "arguments", id, ",\"samples_per_channel\":%" PRIu32, samples_per_channel);
    return result("DAQmxCfgInputBuffer", id, 0);
}
API int32 DAQmxSetReadOverWrite(TaskHandle handle, int32 value) {
    uint64_t id = enter("DAQmxSetReadOverWrite");
    if (!valid(handle) || !owned->input_buffer || owned->overwrite || value != 10159)
        return bad("DAQmxSetReadOverWrite", id, "explicit do-not-overwrite before start");
    owned->overwrite = 1;
    trace("DAQmxSetReadOverWrite", "arguments", id, ",\"value\":%" PRId32, value);
    return result("DAQmxSetReadOverWrite", id, 0);
}
#endif
API int32 DAQmxGetTaskNumChans(TaskHandle handle, uInt32 *count) {
    uint64_t id = enter("DAQmxGetTaskNumChans");
    int software = owned && ((owned->measurement >= 7 && owned->measurement <= 9) || (owned->measurement >= 14 && owned->measurement <= 17));
#ifdef NIDAQMX_FIXTURE_ON_DEMAND
    software = 1;
#endif
    if (software) {
        if (!valid(handle) || owned->timing || owned->input_buffer || owned->overwrite ||
            !owned->channels || owned->tc_pending_setter || owned->counted || owned->started || !count)
            return bad("DAQmxGetTaskNumChans", id, "count queried once after software configuration");
    } else if (!valid(handle) || !owned->overwrite || owned->counted || !count)
        return bad("DAQmxGetTaskNumChans", id, "count queried once after owned configuration");
    owned->counted = 1;
    *count = mode("CountZero") ? 0 : mode("CountHuge") ? 65 :
             mode("CountMismatch") ? owned->channels + 1 : owned->channels;
    trace("DAQmxGetTaskNumChans", "output", id, ",\"count\":%" PRIu32, *count);
    return result("DAQmxGetTaskNumChans", id, 0);
}
API int32 DAQmxGetNthTaskReadChannel(TaskHandle handle, uInt32 index, char *buffer, int32 capacity) {
    uint64_t id = enter("DAQmxGetNthTaskReadChannel");
    if (!valid(handle) || !owned->counted || owned->started || index != owned->named + 1 ||
        index == 0 || index > owned->channels)
        return bad("DAQmxGetNthTaskReadChannel", id, "1-based ordered actual channel enumeration");
    trace("DAQmxGetNthTaskReadChannel", "arguments", id,
        ",\"index\":%" PRIu32 ",\"buffer_null\":%s,\"capacity_i32\":%" PRId32, index, buffer ? "false" : "true", capacity);
    uInt32 actual_index = mode("Reverse") ? owned->channels - index :
                           mode("NameDuplicate") ? 0 : index - 1;
    const char *name = mode("NameForeign") ? "Foreign/ai99" : owned->channel[actual_index];
    size_t needed = strlen(name) + 1;
    if (!buffer && capacity == 0 && owned->pending_name == 0) {
        owned->pending_name = index;
        int32 bytes = mode("NameProbeHuge") ? INT32_MAX : mode("NameProbeZero") ? 0 : (int32)needed;
        trace("DAQmxGetNthTaskReadChannel", "return", id,
            ",\"status\":%" PRId32 ",\"required_bytes\":%" PRId32 ",\"sizing_result\":true", bytes, bytes);
        return bytes; /* Positive size is deliberately NOT a native warning. */
    }
    if (!buffer || owned->pending_name != index || capacity < (int32)needed || capacity > MAX_NAME + 1)
        return bad("DAQmxGetNthTaskReadChannel", id, "bounded size probe followed by real buffer");
    if (mode("NameUnterminated")) memset(buffer, 'X', (size_t)capacity);
    else if (mode("NameInvalidUtf8")) { memset(buffer, 0, (size_t)capacity); buffer[0] = (char)0xff; }
    else memcpy(buffer, name, needed);
    owned->pending_name = 0; ++owned->named;
    trace("DAQmxGetNthTaskReadChannel", "output", id, ",\"index\":%" PRIu32 ",\"name\":\"%s\"", index, name);
    return result("DAQmxGetNthTaskReadChannel", id, 0);
}
#ifndef NIDAQMX_FIXTURE_OMIT_START
API int32 DAQmxStartTask(TaskHandle handle) {
    uint64_t id = enter("DAQmxStartTask");
    if (!valid(handle) || owned->started || owned->named != owned->channels || !owned->channels)
        return bad("DAQmxStartTask", id, "complete validated descriptor before single start");
    if (owned->measurement == 7 && owned->width_mask !=
        (owned->channels == 64 ? UINT64_MAX : (UINT64_C(1) << owned->channels) - 1))
        return bad("DAQmxStartTask", id, "every digital width observed before start");
    for (uInt32 i=0;i<owned->channels;++i)
        if(owned->coupling_expected[i]&&!owned->coupling_set[i])return bad("DAQmxStartTask",id,"every explicit coupling applied before Start");
    owned->started = 1;
    return result("DAQmxStartTask", id, mode("StartBusy") ? INT32_C(-50103) : mode("StartWarn") ? 12345 : 0);
}
#endif
#ifndef NIDAQMX_FIXTURE_ON_DEMAND
API int32 DAQmxGetSampClkRate(TaskHandle handle, float64 *rate) {
    uint64_t id = enter("DAQmxGetSampClkRate");
    if (!valid(handle) || !owned->started || owned->stopped || owned->rate_queried || !rate)
        return bad("DAQmxGetSampClkRate", id, "nominal rate only after owned start");
    *rate = mode("Coerced") ? owned->rate / 2.0 : owned->rate;
    owned->rate_queried = 1;
    trace("DAQmxGetSampClkRate", "output", id, ",\"rate_bits\":\"%016" PRIx64 "\"", bits(*rate));
    return result("DAQmxGetSampClkRate", id, 0);
}
static uInt32 supply(void) { return mode("Max") ? 4096U : mixed_mode() && !mode("Voltage") ? 2U : ((owned->measurement >= 10 && owned->measurement <= 12) || (owned->measurement>=18&&owned->measurement<=23) || (owned->measurement>=25&&owned->measurement<=29)) ? (12U + owned->channels - 1) / owned->channels : 6U; }
API int32 DAQmxGetReadAvailSampPerChan(TaskHandle handle, uInt32 *available) {
    uint64_t id = enter("DAQmxGetReadAvailSampPerChan");
    if (!valid(handle) || !owned->started || !owned->rate_queried || owned->stopped || owned->terminal_read || !available)
        return bad("DAQmxGetReadAvailSampPerChan", id, "availability within owned running task");
    if (mode("AvailHang")) delay_ms(30000);
    if (mode("AvailError")) return result("DAQmxGetReadAvailSampPerChan", id, INT32_C(-200279));
    *available = mode("Idle") ? 0 : mode("AvailHuge") ? UINT32_MAX :
        owned->scans_read >= supply() ? 0 : supply() - (uInt32)owned->scans_read;
    owned->last_available = *available; owned->available_queried = 1;
    trace("DAQmxGetReadAvailSampPerChan", "output", id, ",\"available\":%" PRIu32, *available);
    return result("DAQmxGetReadAvailSampPerChan", id, 0);
}
#endif
#if defined(NIDAQMX_FIXTURE_ENGINEERING_CI_ONLY)
__attribute__((visibility("hidden")))
#else
AI_API
#endif
int32 DAQmxReadAnalogF64(TaskHandle handle, int32 requested, float64 timeout,
        bool32 fill_mode, float64 *array, uInt32 capacity, int32 *actual, bool32 *reserved) {
    uint64_t id = enter("DAQmxReadAnalogF64");
    if(owned&&((owned->measurement>=7&&owned->measurement<=9)||(owned->measurement>=14&&owned->measurement<=17)))
        return bad("DAQmxReadAnalogF64",id,"analog read never accepts a digital/counter task");
    trace("DAQmxReadAnalogF64", "arguments", id,
        ",\"requested\":%" PRId32 ",\"timeout_bits\":\"%016" PRIx64
        "\",\"fill_mode\":%" PRIu32 ",\"capacity\":%" PRIu32 ",\"reserved_null\":%s",
        requested, bits(timeout), fill_mode, capacity, reserved ? "false" : "true");
#ifdef NIDAQMX_FIXTURE_ON_DEMAND
    if (!valid(handle) || !owned->started || owned->stopped || owned->terminal_read ||
        owned->timing || owned->input_buffer || owned->overwrite || owned->rate_queried ||
        owned->available_queried || requested != 1 || !isfinite(timeout) || !(timeout > 0) || timeout > 31 ||
        fill_mode != 1 || !array || !actual || reserved || capacity != owned->channels || capacity > 64)
        return bad("DAQmxReadAnalogF64", id, "fresh software one-scan positive-timeout exact-channel-capacity read");
#else
    if (!valid(handle) || !owned->started || !owned->rate_queried || owned->stopped || owned->terminal_read ||
        !owned->available_queried || requested <= 0 || (uInt32)requested > owned->last_available ||
        bits(timeout) != 0 || fill_mode != 1 || !array || !actual || reserved ||
        (uInt64)(uInt32)requested * owned->channels != capacity || capacity > 4096 ||
        (!mode("AvailHuge") && (uInt64)(uInt32)requested > supply() - owned->scans_read))
        return bad("DAQmxReadAnalogF64", id, "positive bounded scan-major zero-timeout exact-capacity read");
#endif
    for (uInt32 i = 0; i < capacity; ++i)
        if (bits(array[i]) != 0) return bad("DAQmxReadAnalogF64", id, "native allocation must start zeroed");
    ++owned->reads; owned->available_queried = 0; *actual = 0;
    if (mode("ReadCrash")) { trace("DAQmxReadAnalogF64", "crash", id, ",\"exit_code\":86"); _exit(86); }
    if (mode("ReadHang")) delay_ms(30000);
#ifdef NIDAQMX_FIXTURE_ON_DEMAND
    if (mode("SlowRead")) delay_ms(150);
    if (mode("LateRead")) delay_ms(600);
#endif
    int32 status = 0;
    if (mode("CorruptCountNegative")) *actual = -1;
    else if (mode("CorruptCountTooLarge")) *actual = requested + 1;
    else if (mode("ZeroRead") || mode("WarnZeroRead")) {
        *actual = 0; status = mode("WarnZeroRead") ? 12345 : 0;
    }
    else {
        *actual = (mode("Partial") || mode("PartialStopFail") || mode("DiagSlowPartial") ||
                   mode("DiagHangPartial") || mode("DiagError") || mode("DiagInvalidUtf8") || mode("DiagUnterminated") ||
                   mode("Overrun") || mode("FifoOverflow")) ? 1 : requested;
        if (mode("NegativeNoData")) *actual = 0;
        if (mode("Partial") || mode("PartialStopFail") || mode("DiagSlowPartial") || mode("DiagHangPartial") || mode("NegativeNoData") ||
            mode("DiagError") || mode("DiagInvalidUtf8") || mode("DiagUnterminated")) status = -200284;
        else if (mode("Overrun")) status = -200279;
        else if (mode("FifoOverflow")) status = -200361;
        else if ((mode("Warn") || mode("WarnDiagHang")) && owned->reads == 1) status = 12345;
        uInt64 start = owned->scans_read * owned->channels;
        uInt32 count = (uInt32)*actual * owned->channels;
        for (uInt32 i = 0; i < count; ++i) {
            const uint64_t *patterns = owned->measurement==30 ? five_mixed_words : owned->measurement>=28&&owned->measurement<=29 ? position_words : owned->measurement>=25&&owned->measurement<=27 ? dynamic_words : owned->measurement==24 ? resistive_mixed_words : owned->measurement>=18&&owned->measurement<=20 ? resistive_words : owned->measurement>=21&&owned->measurement<=23 ? physical_bridge_words : owned->measurement == 13 ? engineering_mixed_words : owned->measurement >= 10 && owned->measurement <= 12 ? engineering_words : owned->measurement == 5 ? mixed_words : owned->measurement == 4 ? rtd_words : owned->measurement == 3 ? current_words : owned->measurement == 2 ? tc_words : words;
            uint64_t word = patterns[(start + i) % (owned->measurement==30 ? 36 : owned->measurement==24 ? 26 : owned->measurement == 13 ? 14 : owned->measurement == 5 ? 8 : 12)];
            memcpy(array + i, &word, sizeof(word));
        }
        owned->scans_read += (uInt32)*actual;
    }
    slow_diagnostic = mode("DiagSlowPartial");
    owned->terminal_read = status < 0 || *actual < 0 || *actual > requested;
    last_status = status;
    trace("DAQmxReadAnalogF64", "return", id,
        ",\"status\":%" PRId32 ",\"actual_per_channel\":%" PRId32
        ",\"words\":%" PRIu64 ",\"scans_read\":%" PRIu64,
        status, *actual, *actual < 0 || *actual > requested ? UINT64_C(0) :
        (uInt64)(uInt32)*actual * owned->channels, owned->scans_read);
    return status;
}

/* Additive frozen digital78475/count-frequencya056 native-word controls. */
static const uint32_t digital_words[12] = {
    UINT32_C(0x00000001), UINT32_C(0x000000a5), UINT32_C(0x80000000),
    UINT32_C(0x00000000), UINT32_C(0x0000005a), UINT32_C(0xffffffff),
    UINT32_C(0x00000001), UINT32_C(0x00000081), UINT32_C(0x00000000),
    UINT32_C(0x00000001), UINT32_C(0x00000081), UINT32_C(0x00000000)
};
static const uint32_t count_words[8] = {
    UINT32_C(0x00000000), UINT32_C(0x80000000), UINT32_C(0xfffffffe), UINT32_C(0xffffffff),
    UINT32_C(0x00000000), UINT32_C(0x00000001), UINT32_C(0x00000001), UINT32_C(0xffffffff)
};
static const uint64_t frequency_words[8] = {
    UINT64_C(0x4039000000000000), UINT64_C(0x3f90000000000000),
    UINT64_C(0x0000000000000000), UINT64_C(0x8000000000000000),
    UINT64_C(0x7ff80000000000a5), UINT64_C(0x7ff0000000000000),
    UINT64_C(0xfff0000000000000), UINT64_C(0xc004000000000000)
};
static int input_physical(const char *name, const char *kind, char *device, uInt32 *number) {
    if (!name || strnlen(name, MAX_NAME + 1) > MAX_NAME) return 0;
    const char *slash = strchr(name, '/');
    size_t length = slash ? (size_t)(slash - name) : 0;
    if (!length || strncmp(slash + 1, kind, strlen(kind)) != 0) return 0;
    for (const char *q = name; q < slash; ++q)
        if (!((*q >= 'A' && *q <= 'Z') || (*q >= 'a' && *q <= 'z') ||
              (*q >= '0' && *q <= '9') || *q == '_')) return 0;
    const char *digits = slash + 1 + strlen(kind);
    if (!*digits) return 0;
    uint64_t n = 0;
    for (const char *q = digits; *q; ++q) {
        if (*q < '0' || *q > '9') return 0;
        n = n * 10 + (unsigned)(*q - '0');
        if (n > UINT32_MAX) return 0;
    }
    memcpy(device, name, length); device[length] = '\0'; *number = (uInt32)n;
    return 1;
}

#if defined(NIDAQMX_FIXTURE_FIVE_ONLY) || defined(NIDAQMX_FIXTURE_RESISTIVE_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_AI_ONLY) || defined(NIDAQMX_FIXTURE_OMIT_PERIOD)
#define PERIOD_API __attribute__((visibility("hidden")))
#else
#define PERIOD_API API
#endif
#if defined(NIDAQMX_FIXTURE_FIVE_ONLY) || defined(NIDAQMX_FIXTURE_RESISTIVE_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_AI_ONLY) || defined(NIDAQMX_FIXTURE_OMIT_PULSE)
#define PULSE_API __attribute__((visibility("hidden")))
#else
#define PULSE_API API
#endif
#if defined(NIDAQMX_FIXTURE_FIVE_ONLY) || defined(NIDAQMX_FIXTURE_RESISTIVE_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_AI_ONLY) || defined(NIDAQMX_FIXTURE_OMIT_ANGULAR)
#define ANGULAR_API __attribute__((visibility("hidden")))
#else
#define ANGULAR_API API
#endif
#if defined(NIDAQMX_FIXTURE_FIVE_ONLY) || defined(NIDAQMX_FIXTURE_RESISTIVE_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_AI_ONLY) || defined(NIDAQMX_FIXTURE_OMIT_LINEAR)
#define LINEAR_API __attribute__((visibility("hidden")))
#else
#define LINEAR_API API
#endif
#if defined(NIDAQMX_FIXTURE_FIVE_ONLY) || defined(NIDAQMX_FIXTURE_RESISTIVE_ONLY) || defined(NIDAQMX_FIXTURE_ENGINEERING_AI_ONLY)
#define ENCODER_API __attribute__((visibility("hidden")))
#else
#define ENCODER_API API
#endif
static int32 engineering_ci_begin(const char *fn,uint64_t id,TaskHandle handle,const char *name,const char *assigned,int kind) {
    char device[MAX_NAME+1];uInt32 number;
    if (!valid(handle) || owned->channels || owned->timing || owned->started || !empty(assigned) || !input_physical(name,"ctr",device,&number))
        return bad(fn,id,"one fresh individual engineering counter/empty virtual");
    const uInt32 counters[4]={2,3,5,6};
    if (number != counters[kind-14]) return bad(fn,id,"frozen individual counter ordinal");
    strcpy(scenario,device);owned->measurement=kind;input_profile=1;
    return 0;
}
static int32 engineering_ci_commit(const char *fn,uint64_t id,const char *name) {
    owned->channels=1;strcpy(owned->channel[0],name);strcpy(owned->requested[0],name);
    return result(fn,id,0);
}
PERIOD_API int32 DAQmxCreateCIPeriodChan(TaskHandle handle,const char *name,const char *assigned,
        float64 minimum,float64 maximum,int32 units,int32 edge,int32 method,float64 time,uInt32 divisor,const char *scale) {
    uint64_t id=enter("DAQmxCreateCIPeriodChan");
    int32 setup=engineering_ci_begin("DAQmxCreateCIPeriodChan",id,handle,name,assigned,14);if(setup)return setup;
    int32 expected_method=mode("Low") ? 10105 : mode("Large") ? 10205 : 10157;
    uint64_t expected_time=mode("Low") ? UINT64_C(0x3eb0c6f7a0b5ed8d) : UINT64_C(0x3fa0000000000000);
    trace("DAQmxCreateCIPeriodChan","arguments",id,",\"physical\":\"%s\",\"virtual_empty\":true,\"minimum_bits\":\"%016" PRIx64
        "\",\"maximum_bits\":\"%016" PRIx64 "\",\"units\":%" PRId32 ",\"edge\":%" PRId32 ",\"method\":%" PRId32
        ",\"measurement_time_bits\":\"%016" PRIx64 "\",\"divisor_u32\":%" PRIu32 ",\"custom_scale_null\":%s",
        name,bits(minimum),bits(maximum),units,edge,method,bits(time),divisor,scale ? "false":"true");
    if(bits(minimum)!=UINT64_C(0x3f50000000000000)||bits(maximum)!=UINT64_C(0x3fc0000000000000)||units!=10364||edge!=10171||
        method!=expected_method||bits(time)!=expected_time||divisor!=(mode("Large")?7U:4U)||scale)
        return bad("DAQmxCreateCIPeriodChan",id,"frozen Period11 and C1e-6/4, no wrapper dummy rewrite");
    return engineering_ci_commit("DAQmxCreateCIPeriodChan",id,name);
}
PULSE_API int32 DAQmxCreateCIPulseWidthChan(TaskHandle handle,const char *name,const char *assigned,
        float64 minimum,float64 maximum,int32 units,int32 edge,const char *scale) {
    uint64_t id=enter("DAQmxCreateCIPulseWidthChan");
    int32 setup=engineering_ci_begin("DAQmxCreateCIPulseWidthChan",id,handle,name,assigned,15);if(setup)return setup;
    trace("DAQmxCreateCIPulseWidthChan","arguments",id,",\"physical\":\"%s\",\"virtual_empty\":true,\"minimum_bits\":\"%016" PRIx64
        "\",\"maximum_bits\":\"%016" PRIx64 "\",\"units\":%" PRId32 ",\"edge\":%" PRId32 ",\"custom_scale_null\":%s",name,bits(minimum),bits(maximum),units,edge,scale?"false":"true");
    if(bits(minimum)!=UINT64_C(0x3f40000000000000)||bits(maximum)!=UINT64_C(0x3fc8000000000000)||units!=10364||edge!=(mode("Falling")?10171:10280)||scale)
        return bad("DAQmxCreateCIPulseWidthChan",id,"frozen PulseWidth8/seconds/edge/NULLscale");
    return engineering_ci_commit("DAQmxCreateCIPulseWidthChan",id,name);
}
static int32 engineering_encoder(const char *fn,uint64_t id,TaskHandle handle,const char *name,const char *assigned,
        int kind,int32 decoding,bool32 enabled,float64 z,int32 phase,int32 units,uInt32 ppr,float64 distance,float64 initial,const char *scale) {
    int32 setup=engineering_ci_begin(fn,id,handle,name,assigned,kind);if(setup)return setup;
    int linear=kind==17;
    int32 expected_decode=mode("TwoPulse")?10313:mode("X1")?10090:mode("X2")?10091:mode("X4")?10092:linear?10091:10092;
    int32 expected_phase=(mode("PhaseHH")||mode("X1"))?10040:(mode("PhaseHL")||mode("X2"))?10041:(mode("PhaseLH")||mode("X4"))?10042:mode("PhaseLL")||mode("Disabled")||mode("TwoPulse")?10043:linear?10041:10042;
    uint64_t z_bits=mode("Disabled")?UINT64_C(0x3fd8000000000000):linear?UINT64_C(0x3ff6000000000000):UINT64_C(0x3ffc000000000000);
    trace(fn,"arguments",id,",\"physical\":\"%s\",\"virtual_empty\":true,\"decoding\":%" PRId32 ",\"z_enabled_bool32\":%" PRIu32
        ",\"z_bits\":\"%016" PRIx64 "\",\"z_phase\":%" PRId32 ",\"units\":%" PRId32 ",\"pulses_per_rev_u32\":%" PRIu32
        ",\"distance_bits\":\"%016" PRIx64 "\",\"initial_bits\":\"%016" PRIx64 "\",\"custom_scale_null\":%s",name,decoding,enabled,bits(z),phase,units,ppr,bits(distance),bits(initial),scale?"false":"true");
    if(decoding!=expected_decode||enabled!=(mode("Disabled")?0U:1U)||bits(z)!=z_bits||phase!=expected_phase||scale)
        return bad(fn,id,"explicit fixed bool32/Z/phase/decode inputs");
    if(linear) {
        if(units!=(mode("Inches")?10379:10219)||bits(distance)!=(mode("Inches")?UINT64_C(0x3f58000000000000):UINT64_C(0x3f50000000000000))||
            bits(initial)!=(mode("Inches")?UINT64_C(0xbfec000000000000):UINT64_C(0x3ff4000000000000)))return bad(fn,id,"linear F64DPP/initial/unit slots");
    } else if(units!=(mode("Radians")?10273:10146)||ppr!=4099||bits(initial)!=UINT64_C(0xc004000000000000))return bad(fn,id,"angular U32PPR/initial/unit slots");
    return engineering_ci_commit(fn,id,name);
}
ANGULAR_API int32 DAQmxCreateCIAngEncoderChan(TaskHandle h,const char*n,const char*a,int32 d,bool32 e,float64 z,int32 p,int32 u,uInt32 r,float64 i,const char*s) {
    uint64_t id=enter("DAQmxCreateCIAngEncoderChan");return engineering_encoder("DAQmxCreateCIAngEncoderChan",id,h,n,a,16,d,e,z,p,u,r,0.0,i,s);
}
LINEAR_API int32 DAQmxCreateCILinEncoderChan(TaskHandle h,const char*n,const char*a,int32 d,bool32 e,float64 z,int32 p,int32 u,float64 r,float64 i,const char*s) {
    uint64_t id=enter("DAQmxCreateCILinEncoderChan");return engineering_encoder("DAQmxCreateCILinEncoderChan",id,h,n,a,17,d,e,z,p,u,0,r,i,s);
}
static int32 engineering_terminal(const char*fn,uint64_t id,TaskHandle h,const char*n,const char*t,unsigned bit,int pfi) {
    char expected[MAX_NAME+16];
    if(!valid(h)||owned->measurement<14||owned->started||!n||strcmp(n,owned->requested[0])||!t||(owned->terminal_set&bit))return bad(fn,id,"exact selected source once in owned setup");
    if(bit==4&&mode("Disabled"))return bad(fn,id,"disabled Z has no requested source");
    snprintf(expected,sizeof(expected),"/%s/PFI%d",scenario,pfi);
    if(strcmp(t,expected))return bad(fn,id,"frozen explicit source bytes, no pin inference");
    owned->terminal_set|=bit;trace(fn,"arguments",id,",\"channel\":\"%s\",\"terminal\":\"%s\"",n,t);
    return result(fn,id,mode("TermError")?INT32_C(-990401):0);
}
#ifdef NIDAQMX_FIXTURE_OMIT_PERIOD_TERMINAL
#define PERIOD_TERMINAL_API __attribute__((visibility("hidden")))
#else
#define PERIOD_TERMINAL_API PERIOD_API
#endif
PERIOD_TERMINAL_API int32 DAQmxSetCIPeriodTerm(TaskHandle h,const char*n,const char*t){uint64_t id=enter("DAQmxSetCIPeriodTerm");return engineering_terminal("DAQmxSetCIPeriodTerm",id,h,n,t,1,5);}
#ifdef NIDAQMX_FIXTURE_OMIT_PULSE_TERMINAL
#define PULSE_TERMINAL_API __attribute__((visibility("hidden")))
#else
#define PULSE_TERMINAL_API PULSE_API
#endif
PULSE_TERMINAL_API int32 DAQmxSetCIPulseWidthTerm(TaskHandle h,const char*n,const char*t){uint64_t id=enter("DAQmxSetCIPulseWidthTerm");return engineering_terminal("DAQmxSetCIPulseWidthTerm",id,h,n,t,1,7);}
#ifdef NIDAQMX_FIXTURE_OMIT_ENCODER_A
#define ENCODER_A_API __attribute__((visibility("hidden")))
#else
#define ENCODER_A_API ENCODER_API
#endif
ENCODER_A_API int32 DAQmxSetCIEncoderAInputTerm(TaskHandle h,const char*n,const char*t){uint64_t id=enter("DAQmxSetCIEncoderAInputTerm");return engineering_terminal("DAQmxSetCIEncoderAInputTerm",id,h,n,t,1,1);}
#ifdef NIDAQMX_FIXTURE_OMIT_ENCODER_B
#define ENCODER_B_API __attribute__((visibility("hidden")))
#else
#define ENCODER_B_API ENCODER_API
#endif
ENCODER_B_API int32 DAQmxSetCIEncoderBInputTerm(TaskHandle h,const char*n,const char*t){uint64_t id=enter("DAQmxSetCIEncoderBInputTerm");return engineering_terminal("DAQmxSetCIEncoderBInputTerm",id,h,n,t,2,3);}
#ifdef NIDAQMX_FIXTURE_OMIT_ENCODER_Z
#define ENCODER_Z_API __attribute__((visibility("hidden")))
#else
#define ENCODER_Z_API ENCODER_API
#endif
ENCODER_Z_API int32 DAQmxSetCIEncoderZInputTerm(TaskHandle h,const char*n,const char*t){uint64_t id=enter("DAQmxSetCIEncoderZInputTerm");return engineering_terminal("DAQmxSetCIEncoderZInputTerm",id,h,n,t,4,9);}

#ifndef NIDAQMX_FIXTURE_OMIT_DI
DI_API int32 DAQmxCreateDIChan(TaskHandle handle, const char *name, const char *assigned, int32 grouping) {
    uint64_t id = enter("DAQmxCreateDIChan");
    char device[MAX_NAME + 1], expected[32]; uInt32 port;
    if (!valid(handle) || owned->timing || owned->started || owned->counted || owned->channels >= 3 ||
        (owned->measurement && owned->measurement != 7) || !input_physical(name, "port", device, &port))
        return bad("DAQmxCreateDIChan", id, "one fresh whole-port task with individual names");
    if (!owned->channels) { strcpy(scenario, device); input_profile = 1; }
    uInt32 index = owned->channels;
    const uInt32 ports[3] = {3, 1, 12}, widths[3] = {1, 8, 32};
    snprintf(expected, sizeof(expected), "cfdp%04" PRIu32, index);
    if (strcmp(scenario, device) || port != ports[index] || !assigned || strcmp(assigned, expected) || grouping != 1)
        return bad("DAQmxCreateDIChan", id, "frozen port3/1/12 to cfdp0000/1/2 AllLines");
    trace("DAQmxCreateDIChan", "arguments", id,
          ",\"physical\":\"%s\",\"assigned\":\"%s\",\"grouping\":%" PRId32 ",\"index\":%" PRIu32,
          name, assigned, grouping, index);
    owned->measurement = 7; owned->width[index] = widths[index];
    strcpy(owned->requested[index], name); strcpy(owned->channel[index], assigned); ++owned->channels;
    return result("DAQmxCreateDIChan", id, 0);
}
#endif
#ifndef NIDAQMX_FIXTURE_OMIT_DI_WIDTH
DI_API int32 DAQmxGetDINumLines(TaskHandle handle, const char *name, uInt32 *width) {
    uint64_t id = enter("DAQmxGetDINumLines");
    if (!valid(handle) || owned->measurement != 7 || !owned->counted || owned->named != owned->channels || owned->started || !name || !width)
        return bad("DAQmxGetDINumLines", id, "digital width lookup during setup by assigned name");
    uInt32 index = 0;
    while (index < owned->channels && strcmp(owned->channel[index], name)) ++index;
    if (index == owned->channels || owned->named <= index || (owned->width_mask & (UINT64_C(1) << index)))
        return bad("DAQmxGetDINumLines", id, "one width lookup for each enumerated actual channel");
    owned->width_mask |= UINT64_C(1) << index;
    if (mode("WidthError")) return result("DAQmxGetDINumLines", id, INT32_C(-990301));
    *width = mode("WidthZero") ? 0 : mode("WidthHuge") ? 33 : owned->width[index];
    trace("DAQmxGetDINumLines", "output", id, ",\"channel\":\"%s\",\"width\":%" PRIu32, name, *width);
    return result("DAQmxGetDINumLines", id, 0);
}
#endif
#ifndef NIDAQMX_FIXTURE_OMIT_COUNT
COUNT_API int32 DAQmxCreateCICountEdgesChan(TaskHandle handle, const char *name, const char *assigned,
        int32 edge, uInt32 initial, int32 direction) {
    uint64_t id = enter("DAQmxCreateCICountEdgesChan");
    char device[MAX_NAME + 1]; uInt32 counter;
    if (!valid(handle) || owned->channels || owned->timing || owned->started ||
        !empty(assigned) || !input_physical(name, "ctr", device, &counter))
        return bad("DAQmxCreateCICountEdgesChan", id, "one fresh counter and empty virtual name");
    strcpy(scenario, device); input_profile = 1;
    if (counter != (mode("Up") ? 2U : 7U) || edge != (mode("Up") ? 10280 : 10171) ||
        initial != (mode("Up") ? 17U : UINT32_C(0x80000001)) || direction != (mode("Up") ? 10128 : 10124))
        return bad("DAQmxCreateCICountEdgesChan", id, "frozen six slots/unsigned initial/edge/direction");
    trace("DAQmxCreateCICountEdgesChan", "arguments", id,
        ",\"physical\":\"%s\",\"virtual_empty\":true,\"edge\":%" PRId32
        ",\"initial_u32\":%" PRIu32 ",\"direction\":%" PRId32, name, edge, initial, direction);
    owned->measurement = 8; owned->channels = 1;
    strcpy(owned->channel[0], name); strcpy(owned->requested[0], name);
    return result("DAQmxCreateCICountEdgesChan", id, 0);
}
#endif
#ifndef NIDAQMX_FIXTURE_OMIT_FREQUENCY
FREQ_API int32 DAQmxCreateCIFreqChan(TaskHandle handle, const char *name, const char *assigned,
        float64 minimum, float64 maximum, int32 units, int32 edge, int32 method,
        float64 time, uInt32 divisor, const char *scale) {
    uint64_t id = enter("DAQmxCreateCIFreqChan");
    char device[MAX_NAME + 1]; uInt32 counter;
    if (!valid(handle) || owned->channels || owned->timing || owned->started ||
        !empty(assigned) || !input_physical(name, "ctr", device, &counter))
        return bad("DAQmxCreateCIFreqChan", id, "one fresh frequency counter and empty virtual name");
    strcpy(scenario, device); input_profile = 1;
    const int32 methods[4] = {10105, 10157, 10205, 16065};
    int expected = 10205;
    if (strncmp(scenario, "FrequencyMethod", 15) == 0 && strlen(scenario) == 16 && scenario[15] >= '0' && scenario[15] <= '3')
        expected = methods[(unsigned)(scenario[15] - '0')];
    if (counter != 3 || bits(minimum) != UINT64_C(0x3fc0000000000000) ||
        bits(maximum) != UINT64_C(0x40c0000000000000) || units != 10373 || edge != 10171 ||
        method != expected || bits(time) != UINT64_C(0x3fb0000000000000) || divisor != 7 || scale != NULL)
        return bad("DAQmxCreateCIFreqChan", id, "frozen eleven frequency slots/Hz/NULLscale/binary args");
    trace("DAQmxCreateCIFreqChan", "arguments", id,
        ",\"physical\":\"%s\",\"virtual_empty\":true,\"minimum_bits\":\"%016" PRIx64
        "\",\"maximum_bits\":\"%016" PRIx64 "\",\"units\":%" PRId32 ",\"edge\":%" PRId32
        ",\"method\":%" PRId32 ",\"measurement_time_bits\":\"%016" PRIx64
        "\",\"divisor_u32\":%" PRIu32 ",\"custom_scale_null\":true",
        name, bits(minimum), bits(maximum), units, edge, method, bits(time), divisor);
    owned->measurement = 9; owned->channels = 1;
    strcpy(owned->channel[0], name); strcpy(owned->requested[0], name);
    return result("DAQmxCreateCIFreqChan", id, 0);
}
#endif
static int32 input_terminal(const char *fn, uint64_t id, TaskHandle handle,
        const char *name, const char *terminal, int expected_kind) {
    char expected[MAX_NAME + 16];
    if (!valid(handle) || owned->measurement != expected_kind || owned->started || owned->counted ||
        owned->terminal_set || !name || strcmp(name, owned->requested[0]) || !terminal)
        return bad(fn, id, "selected counter setter immediately in owned setup/exact requested channel");
    snprintf(expected, sizeof(expected), "/%s/PFI%d", scenario, expected_kind == 8 ? 3 : 11);
    if (strcmp(terminal, expected)) return bad(fn, id, "exact requested input terminal, no inferred pin");
    owned->terminal_set = 1;
    trace(fn, "arguments", id, ",\"channel\":\"%s\",\"terminal\":\"%s\"", name, terminal);
    return result(fn, id, mode("TermError") ? INT32_C(-990401) : 0);
}
#ifndef NIDAQMX_FIXTURE_OMIT_COUNT_TERMINAL
COUNT_API int32 DAQmxSetCICountEdgesTerm(TaskHandle handle, const char *name, const char *terminal) {
    uint64_t id = enter("DAQmxSetCICountEdgesTerm");
    return input_terminal("DAQmxSetCICountEdgesTerm", id, handle, name, terminal, 8);
}
#endif
#ifndef NIDAQMX_FIXTURE_OMIT_FREQUENCY_TERMINAL
FREQ_API int32 DAQmxSetCIFreqTerm(TaskHandle handle, const char *name, const char *terminal) {
    uint64_t id = enter("DAQmxSetCIFreqTerm");
    return input_terminal("DAQmxSetCIFreqTerm", id, handle, name, terminal, 9);
}
#endif
static int32 input_read(const char *fn, uint64_t id, TaskHandle handle, int32 requested,
        float64 timeout, void *array, uInt32 capacity, int32 *actual, bool32 *reserved, int kind) {
    trace(fn, "arguments", id, ",\"requested\":%" PRId32 ",\"timeout_bits\":\"%016" PRIx64
          "\",\"capacity\":%" PRIu32 ",\"reserved_null\":%s", requested, bits(timeout), capacity, reserved ? "false" : "true");
    if (!valid(handle) || owned->measurement != kind || !owned->started || owned->stopped || owned->terminal_read ||
        owned->timing || owned->input_buffer || owned->overwrite || owned->available_queried || owned->rate_queried ||
        requested != 1 || !isfinite(timeout) || !(timeout > 0) || timeout > 31 ||
        !array || !actual || reserved || capacity != owned->channels || capacity > 64)
        return bad(fn, id, "positive original software one-scan exact typed capacity/reserved slots");
    for (uInt32 i = 0; i < capacity; ++i)
        if (kind >= 9 ? bits(((float64 *)array)[i]) != 0 : ((uInt32 *)array)[i] != 0)
            return bad(fn, id, "typed native output starts zero initialized");
    ++owned->reads; *actual = 0;
    if (mode("ReadHang")) delay_ms(30000);
    if (mode("SlowRead")) delay_ms(150);
    if (mode("LateRead")) delay_ms(600);
    int32 status = 0;
    if (mode("CorruptCountNegative")) *actual = -1;
    else if (mode("CorruptCountTooLarge")) *actual = 2;
    else if (mode("ZeroRead") || mode("WarnZeroRead") || mode("NegativeNoData")) {
        *actual = 0; status = mode("NegativeNoData") ? -200284 : mode("WarnZeroRead") ? 12345 : 0;
    } else {
        *actual = 1;
        if (mode("Partial") || mode("PartialStopFail") || mode("DiagSlowPartial") || mode("DiagHangPartial")) status = -200284;
        else if ((mode("Warn") || mode("WarnDiagHang")) && owned->reads == 1) status = 12345;
        for (uInt32 i = 0; i < capacity; ++i) {
            uInt64 n = owned->scans_read * owned->channels + i;
            if (kind == 7) ((uInt32 *)array)[i] = digital_words[n % 12];
            else if (kind == 8) ((uInt32 *)array)[i] = count_words[n % 8];
            else { uint64_t word = kind >= 14 ? engineering_counter_words[n % 8] : frequency_words[n % 8]; memcpy((float64 *)array + i, &word, sizeof(word)); }
        }
        ++owned->scans_read;
    }
    owned->terminal_read = status < 0 || *actual != 1; slow_diagnostic = mode("DiagSlowPartial"); last_status = status;
    trace(fn, "return", id, ",\"status\":%" PRId32 ",\"actual_per_channel\":%" PRId32
          ",\"words\":%" PRIu32 ",\"scans_read\":%" PRIu64, status, *actual, *actual == 1 ? capacity : 0, owned->scans_read);
    return status;
}
#ifndef NIDAQMX_FIXTURE_OMIT_DI_READ
DI_API int32 DAQmxReadDigitalU32(TaskHandle handle, int32 requested, float64 timeout, bool32 fill,
        uInt32 *array, uInt32 capacity, int32 *actual, bool32 *reserved) {
    uint64_t id = enter("DAQmxReadDigitalU32");
    trace("DAQmxReadDigitalU32", "fill", id, ",\"fill_mode\":%" PRIu32, fill);
    if (fill != 1) return bad("DAQmxReadDigitalU32", id, "digital fillMode is scan-major one");
    return input_read("DAQmxReadDigitalU32", id, handle, requested, timeout, array, capacity, actual, reserved, 7);
}
#endif
#ifndef NIDAQMX_FIXTURE_OMIT_COUNT_READ
COUNT_API int32 DAQmxReadCounterU32(TaskHandle handle, int32 requested, float64 timeout,
        uInt32 *array, uInt32 capacity, int32 *actual, bool32 *reserved) {
    uint64_t id = enter("DAQmxReadCounterU32");
    return input_read("DAQmxReadCounterU32", id, handle, requested, timeout, array, capacity, actual, reserved, 8);
}
#endif
#ifndef NIDAQMX_FIXTURE_OMIT_FREQUENCY_READ
#if defined(NIDAQMX_FIXTURE_ENGINEERING_AI_ONLY) || defined(NIDAQMX_FIXTURE_RESISTIVE_ONLY)
__attribute__((visibility("hidden")))
#elif defined(NIDAQMX_FIXTURE_ENGINEERING_CI_ONLY)
API
#else
FREQ_API
#endif
int32 DAQmxReadCounterF64(TaskHandle handle, int32 requested, float64 timeout,
        float64 *array, uInt32 capacity, int32 *actual, bool32 *reserved) {
    uint64_t id = enter("DAQmxReadCounterF64");
    return input_read("DAQmxReadCounterF64", id, handle, requested, timeout, array, capacity, actual, reserved, owned && owned->measurement >= 14 && owned->measurement <= 17 ? owned->measurement : 9);
}
#endif

API int32 DAQmxStopTask(TaskHandle handle) {
    uint64_t id = enter("DAQmxStopTask");
    if (!valid(handle) || !owned->started || owned->stop_attempted)
        return bad("DAQmxStopTask", id, "one stop of the actually started owned task");
    owned->stop_attempted = 1;
    if (mode("StopHang")) delay_ms(30000);
    int32 status = mode("PartialStopFail") || mode("StopError") || mode("StopAndClearError") ? -990201 : 0;
    if (status == 0) owned->stopped = 1;
    return result("DAQmxStopTask", id, status);
}
API int32 DAQmxClearTask(TaskHandle handle) {
    uint64_t id = enter("DAQmxClearTask");
    if (!valid(handle) || (owned->started && !owned->stop_attempted))
        return bad("DAQmxClearTask", id, "one clear of qualified owned task after stop attempt");
    if (mode("ClearHang")) delay_ms(30000);
    int32 status = mode("ClearError") || mode("StopAndClearError") ? -990202 : 0;
    int32 returned = result("DAQmxClearTask", id, status);
    free(owned); owned = NULL;
    return returned;
}
static int32 diagnostic(const char *fn, char *buffer, uInt32 capacity, int32 code, int extended) {
    uint64_t id = enter(fn);
    trace(fn, "arguments", id, ",\"code\":%" PRId32 ",\"capacity\":%" PRIu32, code, capacity);
    if (!buffer || capacity == 0 || capacity > 2048) return bad(fn, id, "bounded diagnostic buffer");
#ifdef NIDAQMX_FIXTURE_SLOW_DIAGNOSTICS
    if (slow_once) { slow_once = 0; delay_ms(150); }
#endif
    if (extended && slow_diagnostic) { slow_diagnostic = 0; delay_ms(150); }
    if ((extended && mode("DiagHangPartial")) ||
        (!extended && mode("WarnDiagHang") && code == 12345)) delay_ms(30000);
    if (mode("DiagError")) {
        trace(fn, "return", id, ",\"status\":-990301");
        return -990301;
    }
    int length = snprintf(buffer, capacity, "fixture %s native status %" PRId32,
                          extended ? "extended" : "error-string", code);
    if (length < 0 || (uInt32)length >= capacity) return bad(fn, id, "diagnostic extent");
    if (mode("DiagInvalidUtf8") || mode("DiagSlowPartial")) {
        if ((uInt32)length + 2 > capacity) return bad(fn, id, "diagnostic raw suffix extent");
        buffer[length] = (char)0xff; buffer[length + 1] = '\0';
    }
    if (mode("DiagUnterminated")) memset(buffer, 'X', capacity);
    trace(fn, "return", id, ",\"status\":0,\"original_code\":%" PRId32, code);
    return 0; /* Diagnostic lookup never replaces the original thread-local code. */
}
API int32 DAQmxGetErrorString(int32 code, char *buffer, uInt32 capacity) {
    return diagnostic("DAQmxGetErrorString", buffer, capacity, code, 0);
}
API int32 DAQmxGetExtendedErrorInfo(char *buffer, uInt32 capacity) {
    return diagnostic("DAQmxGetExtendedErrorInfo", buffer, capacity, last_status, 1);
}
__attribute__((destructor)) static void unload(void) {
    trace("fixture_library", "unload", ++sequence, ",\"remaining_task\":%s", owned ? "true" : "false");
    if (owned) { free(owned); owned = NULL; }
    if (trace_fd >= 0) { close(trace_fd); trace_fd = -1; }
}
