// LD_PRELOAD helper for `just rt-count`: counts malloc/calloc/realloc/free
// calls made on the PipeWire/JACK audio thread (identified by thread name) and
// prints the totals when the process exits. Unlike VIBEMIX_RT_GUARD, which only
// sees Rust allocations, this also catches C/C++ code in the plugin shims and
// in hosted plugins.
#define _GNU_SOURCE
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include <stdatomic.h>

extern void *__libc_malloc(size_t);
extern void *__libc_calloc(size_t, size_t);
extern void *__libc_realloc(void *, size_t);
extern void __libc_free(void *);

static __thread int is_rt = -1;
static atomic_long mallocs, frees;

static int rt(void) {
    if (is_rt < 0) {
        char name[32] = {0};
        pthread_getname_np(pthread_self(), name, sizeof name);
        is_rt = (strstr(name, "data-loop") || strstr(name, "jack") || strstr(name, "pw-data")) ? 1 : 0;
    }
    return is_rt;
}
void *malloc(size_t n) { if (rt()) atomic_fetch_add(&mallocs, 1); return __libc_malloc(n); }
void *calloc(size_t a, size_t b) { if (rt()) atomic_fetch_add(&mallocs, 1); return __libc_calloc(a, b); }
void *realloc(void *p, size_t n) { if (rt()) atomic_fetch_add(&mallocs, 1); return __libc_realloc(p, n); }
void free(void *p) { if (p && rt()) atomic_fetch_add(&frees, 1); __libc_free(p); }

__attribute__((destructor)) static void report(void) {
    fprintf(stderr, "RTCOUNT mallocs=%ld frees=%ld\n", atomic_load(&mallocs), atomic_load(&frees));
}
