#include "risp_rt.h"

#include <stdlib.h>
#include <string.h>

struct RispString {
    int refcnt;
    int len;
    char data[];
};

RispString *risp_str_from_cstr(const char *s) {
    if (!s) {
        s = "";
    }
    size_t n = strlen(s);
    RispString *out = (RispString *)malloc(sizeof(RispString) + n + 1);
    if (!out) {
        abort();
    }
    out->refcnt = 1;
    out->len = (int)n;
    memcpy(out->data, s, n + 1);
    return out;
}

RispString *risp_str_retain(RispString *s) {
    if (s) {
        s->refcnt++;
    }
    return s;
}

void risp_str_release(RispString *s) {
    if (!s) {
        return;
    }
    s->refcnt--;
    if (s->refcnt <= 0) {
        free(s);
    }
}

RispString *risp_str_concat(RispString *a, RispString *b) {
    int al = a ? a->len : 0;
    int bl = b ? b->len : 0;
    size_t n = (size_t)al + (size_t)bl;
    RispString *out = (RispString *)malloc(sizeof(RispString) + n + 1);
    if (!out) {
        abort();
    }
    out->refcnt = 1;
    out->len = (int)n;
    if (a && al) {
        memcpy(out->data, a->data, (size_t)al);
    }
    if (b && bl) {
        memcpy(out->data + al, b->data, (size_t)bl);
    }
    out->data[n] = '\0';
    return out;
}

int risp_str_len(RispString *s) {
    return s ? s->len : 0;
}

const char *risp_str_cstr(RispString *s) {
    return s ? s->data : "";
}
