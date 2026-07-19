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

#ifdef __cplusplus
}
#endif

#endif /* RISP_RT_H */
