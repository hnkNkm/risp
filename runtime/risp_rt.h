#ifndef RISP_RT_H
#define RISP_RT_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct RispString RispString;

/** Create a new owned string (+1) from a NUL-terminated C string. */
RispString *risp_str_from_cstr(const char *s);

/** Increment refcount; returns `s`. */
RispString *risp_str_retain(RispString *s);

/** Decrement refcount; frees when it reaches 0. */
void risp_str_release(RispString *s);

/** Concatenate; does not consume `a`/`b`. Returns new owned string (+1). */
RispString *risp_str_concat(RispString *a, RispString *b);

/** Byte length (not including NUL). */
int risp_str_len(RispString *s);

/** Borrowed NUL-terminated pointer for printf/puts. */
const char *risp_str_cstr(RispString *s);

/** Allocate `size` bytes for a `Box` (aborts on OOM). */
void *risp_box_alloc(size_t size);

/** Free a box pointer; no-op when `p` is NULL. */
void risp_box_free(void *p);

/* ---- Vec i32 (unique owned) ---- */

typedef struct RispVecI32 RispVecI32;

RispVecI32 *risp_vec_i32_new(void);
void risp_vec_i32_push(RispVecI32 *v, int x);
int risp_vec_i32_get(RispVecI32 *v, int idx);
int risp_vec_i32_len(RispVecI32 *v);
void risp_vec_i32_free(RispVecI32 *v);

/* ---- Generic Rc / Weak ----
 *
 * Layout: [RispRcHeader][payload...]
 * Language pointers always refer to the payload.
 * Weak and Rc share the same payload address; counts live in the header.
 * When strong hits 0, codegen drops the payload then calls
 * `risp_rc_after_payload_drop`. Weak release may free the block.
 */

/** Allocate header+payload; strong=1, weak=1. Returns payload pointer. */
void *risp_rc_alloc(size_t payload_size);

/** Increment strong; returns payload. */
void *risp_rc_retain(void *payload);

/**
 * Decrement strong; returns new strong count.
 * When the result is 0, caller must drop payload then call
 * `risp_rc_after_payload_drop`.
 */
int risp_rc_release_strong(void *payload);

/** After payload drop when strong==0: decrement weak; free if weak==0. */
void risp_rc_after_payload_drop(void *payload);

/** Create a Weak from Rc (increments weak). Same payload pointer. */
void *risp_weak_from(void *payload);

/**
 * Upgrade Weak → Rc. Returns payload with strong++ if alive, else NULL.
 */
void *risp_weak_upgrade(void *payload);

/** Decrement weak; free block if strong==0 and weak==0. */
void risp_weak_release(void *payload);

#ifdef __cplusplus
}
#endif

#endif /* RISP_RT_H */
