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

void *risp_box_alloc(size_t size) {
    void *p = malloc(size ? size : 1);
    if (!p) {
        abort();
    }
    return p;
}

void risp_box_free(void *p) {
    free(p);
}

/* ---- Vec i32 ---- */

struct RispVecI32 {
    int len;
    int cap;
    int *data;
};

RispVecI32 *risp_vec_i32_new(void) {
    RispVecI32 *v = (RispVecI32 *)malloc(sizeof(RispVecI32));
    if (!v) {
        abort();
    }
    v->len = 0;
    v->cap = 0;
    v->data = NULL;
    return v;
}

void risp_vec_i32_push(RispVecI32 *v, int x) {
    if (!v) {
        abort();
    }
    if (v->len >= v->cap) {
        int ncap = v->cap == 0 ? 4 : v->cap * 2;
        int *nd = (int *)realloc(v->data, (size_t)ncap * sizeof(int));
        if (!nd) {
            abort();
        }
        v->data = nd;
        v->cap = ncap;
    }
    v->data[v->len++] = x;
}

int risp_vec_i32_get(RispVecI32 *v, int idx) {
    if (!v || idx < 0 || idx >= v->len) {
        abort();
    }
    return v->data[idx];
}

int risp_vec_i32_len(RispVecI32 *v) {
    return v ? v->len : 0;
}

void risp_vec_i32_free(RispVecI32 *v) {
    if (!v) {
        return;
    }
    free(v->data);
    free(v);
}

/* ---- Rc / Weak ---- */

typedef struct {
    int strong;
    int weak;
} RispRcHeader;

static RispRcHeader *rc_hdr(void *payload) {
    return ((RispRcHeader *)payload) - 1;
}

void *risp_rc_alloc(size_t payload_size) {
    size_t n = sizeof(RispRcHeader) + (payload_size ? payload_size : 1);
    RispRcHeader *h = (RispRcHeader *)malloc(n);
    if (!h) {
        abort();
    }
    h->strong = 1;
    h->weak = 1; /* allocation holds an implicit weak */
    return (void *)(h + 1);
}

void *risp_rc_retain(void *payload) {
    if (payload) {
        rc_hdr(payload)->strong++;
    }
    return payload;
}

int risp_rc_release_strong(void *payload) {
    if (!payload) {
        return 0;
    }
    RispRcHeader *h = rc_hdr(payload);
    h->strong--;
    return h->strong;
}

void risp_rc_after_payload_drop(void *payload) {
    if (!payload) {
        return;
    }
    RispRcHeader *h = rc_hdr(payload);
    h->weak--;
    if (h->weak <= 0) {
        free(h);
    }
}

void *risp_weak_from(void *payload) {
    if (payload) {
        rc_hdr(payload)->weak++;
    }
    return payload;
}

void *risp_weak_upgrade(void *payload) {
    if (!payload) {
        return NULL;
    }
    RispRcHeader *h = rc_hdr(payload);
    if (h->strong <= 0) {
        return NULL;
    }
    h->strong++;
    return payload;
}

void risp_weak_release(void *payload) {
    if (!payload) {
        return;
    }
    RispRcHeader *h = rc_hdr(payload);
    h->weak--;
    if (h->strong <= 0 && h->weak <= 0) {
        free(h);
    }
}
