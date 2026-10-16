/*
 * fmtio.c -- VISA formatted I/O (VPP-4.3 section 6.2): viPrintf, viScanf, viQueryf and their va_list
 * and buffer forms, viBufWrite / viBufRead, and the formatted I/O buffers they share.
 *
 * These are variadic, and stable Rust can neither define a variadic function nor take a va_list, so
 * this file is C. It moves bytes with the DLL's own viWrite / viRead (Rust); lib.rs calls back in
 * for viFlush, viSetBuf, viClose and the buffer attributes (symbiotic_fio_*).
 *
 * THE FORMAT IS WALKED HERE, one conversion at a time: VISA's own conversions and modifiers are done
 * here, and a plain C conversion is handed to snprintf / strtod with just its own specifier.
 *   write: %b %B (IEEE 488.2 definite / indefinite length blocks: bytes, h 16, l 32, ll 64 bits,
 *          z float, Z double; big-endian on the wire), %y (raw binary, !ob big / !ol little endian),
 *          the ',' array modifier on %d and %f families, the numeric flags @1 @2 @3 @H @Q @B, and
 *          the C conversions d i o u x X c s e E f F g G a A p n %;
 *   read:  %b (definite and indefinite blocks), %y, %t (to END), %T (to LF), '#' sizes passed and
 *          returned by pointer, ',' arrays, NRf and #H #Q #B numbers, and the C conversions
 *          d i o u x X c s [set] e E f F g G a A n %.
 *
 * THE BUFFERS (VPP-4.3 5.1 and 6.2): a session's viPrintf output collects in its write buffer, which
 * is written when the format sends "\n" (LF with END), when it fills (without END: the message goes
 * on), on viFlush(VI_WRITE_BUF), in viQueryf after its write, and -- VI_ATTR_WR_BUF_OPER_MODE
 * VI_FLUSH_ON_ACCESS -- after every call. viScanf parses from the read buffer, reading from the
 * device when it runs out; what a call does not parse stays for the next one (VI_FLUSH_DISABLE,
 * the default) or goes (VI_FLUSH_ON_ACCESS). Within one call, once the message has ended, the
 * remaining conversions are skipped and the call succeeds (OBSERVATION 6.2.8); a new call that
 * finds the buffer used up reads a new message (RULE 6.2.16), including when only white space was
 * left and the conversion skips it (OBSERVATION 6.2.13). A flush writes with END as
 * VI_ATTR_SEND_END_EN says; only a full buffer is written without it.
 *
 * Error values are VISA's, as pyvisa's constants have them.
 */

#include <ctype.h>
#include <limits.h>
#include <stdarg.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifdef _WIN32
#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>
typedef SRWLOCK fio_lock;
#define LOCK(l) AcquireSRWLockExclusive(l)
#define UNLOCK(l) ReleaseSRWLockExclusive(l)
#define LOCK_SETUP(l) InitializeSRWLock(l)
static fio_lock table_lock = SRWLOCK_INIT;
#else
#include <pthread.h>
typedef pthread_mutex_t fio_lock;
#define LOCK(l) pthread_mutex_lock(l)
#define UNLOCK(l) pthread_mutex_unlock(l)
#define LOCK_SETUP(l) pthread_mutex_init(l, NULL)
static fio_lock table_lock = PTHREAD_MUTEX_INITIALIZER;
#endif

typedef uint32_t ViSession;
typedef int32_t ViStatus;
typedef uint32_t ViUInt32;
typedef uint16_t ViUInt16;
typedef uint32_t ViAttr;
/* As wide as a pointer: a ViUInt64 in a 64-bit VISA, a ViUInt32 in a 32-bit one (VPP-4.3). */
#if defined(_WIN64) || defined(__LP64__)
typedef uint64_t ViAttrState;
#else
typedef uint32_t ViAttrState;
#endif

/* VPP-4.3's _VI_FUNC: __stdcall on 32-bit Windows (x64 has one convention). The variadic
 * functions are __cdecl there, as they must be, and are declared without it. */
#if defined(_WIN32) && !defined(_WIN64)
#define VI_API __stdcall
#else
#define VI_API
#endif

#define VI_SUCCESS               ((ViStatus)0)
#define VI_SUCCESS_TERM_CHAR     ((ViStatus)0x3FFF0005)
#define VI_SUCCESS_MAX_CNT       ((ViStatus)0x3FFF0006)
#define VI_ERROR_NSUP_ATTR       ((ViStatus)0xBFFF001D)
#define VI_ERROR_NSUP_ATTR_STATE ((ViStatus)0xBFFF001E)
#define VI_ERROR_ATTR_READONLY   ((ViStatus)0xBFFF001F)
#define VI_ERROR_INV_OBJECT      ((ViStatus)0xBFFF000E)
#define VI_ERROR_ALLOC           ((ViStatus)0xBFFF003C)
#define VI_ERROR_INV_MASK        ((ViStatus)0xBFFF003D)
#define VI_ERROR_INV_FMT         ((ViStatus)0xBFFF003F)
#define VI_ERROR_NSUP_FMT        ((ViStatus)0xBFFF0041)
#define VI_ERROR_INV_PARAMETER   ((ViStatus)0xBFFF0078)

#define VI_ATTR_SEND_END_EN      0x3FFF0016u
#define VI_ATTR_TMO_VALUE        0x3FFF001Au
#define VI_ATTR_RD_BUF_OPER_MODE 0x3FFF002Au
#define VI_ATTR_RD_BUF_SIZE      0x3FFF002Bu
#define VI_ATTR_WR_BUF_OPER_MODE 0x3FFF002Du
#define VI_ATTR_WR_BUF_SIZE      0x3FFF002Eu

#define VI_FLUSH_ON_ACCESS  1
#define VI_FLUSH_WHEN_FULL  2
#define VI_FLUSH_DISABLE    3

#define VI_READ_BUF          1
#define VI_WRITE_BUF         2
#define VI_READ_BUF_DISCARD  4
#define VI_WRITE_BUF_DISCARD 8

#define DEFAULT_BUF 4096

/* The DLL's own, in Rust (lib.rs). symbiotic_op_enter/leave bracket each exported function here, so
 * that a failed call raises VISA's exception event under its own name (viPrintf, not the viWrite it
 * made), and only once. */
extern void symbiotic_op_enter(void);
extern ViStatus symbiotic_op_leave(ViSession vi, ViStatus st, const char *oper);
extern ViStatus VI_API viWrite(ViSession vi, const unsigned char *buf, ViUInt32 count, ViUInt32 *ret);
extern ViStatus VI_API viRead(ViSession vi, unsigned char *buf, ViUInt32 count, ViUInt32 *ret);
extern ViStatus VI_API viGetAttribute(ViSession vi, ViAttr attr, void *value);
extern ViStatus VI_API viSetAttribute(ViSession vi, ViAttr attr, ViAttrState value);

/* ------------------------------------------------------------------------------------------ */
/*  A session's buffers                                                                       */
/* ------------------------------------------------------------------------------------------ */

struct fio {
    ViSession vi;
    struct fio *next;
    fio_lock lock;
    unsigned char *w;
    size_t wn, wsize;
    unsigned char *r;
    size_t rpos, rn, rsize;
    int r_end;            /* the data in r ends a message (END, or the termination character) */
    ViStatus r_why;       /* ...which: VI_SUCCESS or VI_SUCCESS_TERM_CHAR */
    int wmode, rmode;
};

static struct fio *table;

/* The session's buffers, locked; made if `make` and there are none yet (else NULL). */
static struct fio *fio_get(ViSession vi, int make)
{
    LOCK(&table_lock);
    struct fio *f = table;
    while (f && f->vi != vi)
        f = f->next;
    if (!f && make) {
        f = (struct fio *)calloc(1, sizeof *f);
        if (f) {
            f->vi = vi;
            f->wsize = f->rsize = DEFAULT_BUF;
            f->wmode = VI_FLUSH_WHEN_FULL;
            f->rmode = VI_FLUSH_DISABLE;
            LOCK_SETUP(&f->lock);
            f->next = table;
            table = f;
        }
    }
    UNLOCK(&table_lock);
    if (f)
        LOCK(&f->lock);
    return f;
}

/* A session's buffers, for an operation that uses them. The session is checked first, so text is not
   buffered for a session that does not exist (VI_ERROR_INV_OBJECT). NULL with *why. */
static struct fio *fio_open(ViSession vi, ViStatus *why)
{
    ViUInt32 tmo;
    if (viGetAttribute(vi, VI_ATTR_TMO_VALUE, &tmo) == VI_ERROR_INV_OBJECT) {
        *why = VI_ERROR_INV_OBJECT;
        return NULL;
    }
    struct fio *f = fio_get(vi, 1);
    *why = VI_ERROR_ALLOC;
    return f;
}

static void fio_put(struct fio *f)
{
    UNLOCK(&f->lock);
}

static void read_discard(struct fio *f)
{
    f->rpos = f->rn = 0;
    f->r_end = 0;
}

/* One viWrite of the bytes: END with the last as VI_ATTR_SEND_END_EN says if `end`, none if not. */
static ViStatus send_bytes(ViSession vi, const unsigned char *p, size_t n, int end)
{
    ViUInt16 en = 0;
    int toggled = 0;
    if (!end && viGetAttribute(vi, VI_ATTR_SEND_END_EN, &en) == VI_SUCCESS && en) {
        viSetAttribute(vi, VI_ATTR_SEND_END_EN, 0);
        toggled = 1;
    }
    ViUInt32 done = 0;
    ViStatus st = viWrite(vi, p, (ViUInt32)n, &done);
    if (toggled)
        viSetAttribute(vi, VI_ATTR_SEND_END_EN, 1);
    return st < 0 ? st : VI_SUCCESS;
}

/* Write what the buffer holds. It is empty afterwards, whatever happened (RULE 6.2.10). */
static ViStatus wflush(struct fio *f, int end)
{
    if (f->wn == 0)
        return VI_SUCCESS;
    ViStatus st = send_bytes(f->vi, f->w, f->wn, end);
    f->wn = 0;
    return st;
}

static ViStatus wput(struct fio *f, const void *data, size_t n)
{
    const unsigned char *p = (const unsigned char *)data;
    if (!f->w) {
        f->w = (unsigned char *)malloc(f->wsize);
        if (!f->w)
            return VI_ERROR_ALLOC;
    }
    while (n) {
        size_t room = f->wsize - f->wn, k = n < room ? n : room;
        memcpy(f->w + f->wn, p, k);
        f->wn += k;
        p += k;
        n -= k;
        if (f->wn == f->wsize) {
            ViStatus st = wflush(f, 0);    /* full: out, but the message goes on */
            if (st != VI_SUCCESS)
                return st;
        }
    }
    return VI_SUCCESS;
}

/* Read the next piece of a message into the (used-up) read buffer. */
static ViStatus fill(struct fio *f)
{
    if (!f->r) {
        f->r = (unsigned char *)malloc(f->rsize);
        if (!f->r)
            return VI_ERROR_ALLOC;
    }
    f->rpos = f->rn = 0;
    ViUInt32 got = 0;
    ViStatus st = viRead(f->vi, f->r, (ViUInt32)f->rsize, &got);
    f->rn = got;
    if (st == VI_SUCCESS || st == VI_SUCCESS_TERM_CHAR) {
        f->r_end = 1;
        f->r_why = st;
        return VI_SUCCESS;
    }
    f->r_end = 0;
    if (st < 0) {
        if (got == 0)
            read_discard(f);                  /* RULE 6.2.15 */
        return st;
    }
    return VI_SUCCESS;                        /* VI_SUCCESS_MAX_CNT: the message goes on */
}

/* ------------------------------------------------------------------------------------------ */
/*  Format specifiers                                                                         */
/* ------------------------------------------------------------------------------------------ */

enum { L_NONE, L_HH, L_H, L_L, L_LL, L_BIGL, L_J, L_Z, L_T, L_BIGZ };

typedef struct spec {
    char flags[8];
    int nflags;
    char nr;                 /* @1 @2 @3 @H @Q @B, or 0 */
    int has_width, width, width_star, width_hash;
    int has_prec, prec, prec_star;
    int array, has_asize, asize, asize_star, asize_hash;
    int len;
    int little;              /* !ol */
    int suppress;            /* scanf '*' */
    char conv;
    const char *set;         /* %[...]: what is between the brackets */
    size_t nset;
} spec;

static int is_int_conv(char c) { return c && strchr("diouxX", c) != NULL; }
static int is_float_conv(char c) { return c && strchr("fFeEgGaA", c) != NULL; }
static int is_block_conv(char c) { return c == 'b' || c == 'B' || c == 'y'; }

/* The digits at `p` as a count, onto `*v`; a count too large for an int is INT_MAX, not an overflow. */
static const char *parse_count(const char *p, int *v)
{
    while (*p >= '0' && *p <= '9') {
        int d = *p++ - '0';
        *v = *v > (INT_MAX - d) / 10 ? INT_MAX : *v * 10 + d;
    }
    return p;
}

static const char *parse_swap(const char *p, spec *sp)
{
    if (p[0] == '!' && p[1] == 'o' && (p[2] == 'b' || p[2] == 'l')) {
        sp->little = p[2] == 'l';
        return p + 3;
    }
    return p;
}

/* Parse one specifier; `p` is just past the '%'. NULL and *err on a bad one. */
static const char *parse_spec(const char *p, int reading, spec *sp, ViStatus *err)
{
    memset(sp, 0, sizeof *sp);
    *err = VI_ERROR_INV_FMT;
    if (reading && *p == '*') {
        sp->suppress = 1;
        p++;
    }
    for (;;) {
        if (!reading && *p && strchr("-+ #0", *p)) {
            if (sp->nflags < 7)
                sp->flags[sp->nflags++] = *p;
            p++;
        } else if (!reading && *p == '@' && p[1] && strchr("123HQB", p[1])) {
            sp->nr = p[1];
            p += 2;
        } else {
            break;
        }
    }
    if (*p == '*' && !reading) {
        sp->width_star = sp->has_width = 1;
        p++;
    } else if (*p == '#' && reading) {
        sp->width_hash = sp->has_width = 1;
        p++;
    } else if (*p >= '0' && *p <= '9') {
        sp->has_width = 1;
        p = parse_count(p, &sp->width);
    }
    p = parse_swap(p, sp);
    if (*p == '.' && !reading) {
        p++;
        sp->has_prec = 1;
        if (*p == '*') {
            sp->prec_star = 1;
            p++;
        } else {
            p = parse_count(p, &sp->prec);
        }
    }
    if (*p == ',') {
        p++;
        sp->array = 1;
        if (*p == '*' && !reading) {
            sp->asize_star = sp->has_asize = 1;
            p++;
        } else if (*p == '#' && reading) {
            sp->asize_hash = sp->has_asize = 1;
            p++;
        } else if (*p >= '0' && *p <= '9') {
            sp->has_asize = 1;
            p = parse_count(p, &sp->asize);
        }
        if (!sp->has_asize)
            return NULL;
    }
    p = parse_swap(p, sp);
    if (p[0] == 'h' && p[1] == 'h') { sp->len = L_HH; p += 2; }
    else if (*p == 'h') { sp->len = L_H; p++; }
    else if (p[0] == 'l' && p[1] == 'l') { sp->len = L_LL; p += 2; }
    else if (*p == 'l') { sp->len = L_L; p++; }
    else if (*p == 'L') { sp->len = L_BIGL; p++; }
    else if (*p == 'j') { sp->len = L_J; p++; }
    else if (*p == 'z') { sp->len = L_Z; p++; }
    else if (*p == 'Z') { sp->len = L_BIGZ; p++; }
    /* 't' is C's ptrdiff_t length only before an integer conversion; alone it is VISA's %t. */
    else if (*p == 't' && p[1] && strchr("diouxXn", p[1])) { sp->len = L_T; p++; }
    sp->conv = *p;
    if (!sp->conv)
        return NULL;
    p++;
    if (sp->conv == '[') {
        if (!reading)
            return NULL;
        const char *s = p;
        if (*p == '^')
            p++;
        if (*p == ']')
            p++;
        while (*p && *p != ']')
            p++;
        if (!*p)
            return NULL;
        sp->set = s;
        sp->nset = (size_t)(p - s);
        p++;
    }

    /* What goes with what. */
    char c = sp->conv;
    if (is_block_conv(c)) {
        /* %<count>b: the count stands where a width would; no precision, no C flags, no array. */
        if (sp->has_prec || sp->nflags || sp->nr || sp->array)
            return NULL;
        if (!sp->has_width)
            return NULL;
        if (c == 'y' ? (sp->len == L_Z || sp->len == L_BIGZ || sp->len == L_HH || sp->len == L_BIGL || sp->len == L_J || sp->len == L_T)
                     : (sp->len == L_HH || sp->len == L_BIGL || sp->len == L_J || sp->len == L_T)) {
            *err = VI_ERROR_NSUP_FMT;
            return NULL;
        }
        if (c != 'y' && sp->little)
            return NULL;                        /* byte order is IEEE 488.2's for a block */
        if (reading && c == 'B')
            return NULL;                        /* %b reads both kinds of block */
        return p;
    }
    if (sp->little)
        return NULL;
    if (sp->len == L_BIGZ)
        return NULL;                            /* Z is only for blocks */
    if (sp->nr && !is_int_conv(c) && !is_float_conv(c))
        return NULL;
    if (sp->array && !is_int_conv(c) && !is_float_conv(c))
        return NULL;
    if (reading) {
        if (c == 't' || c == 'T' || c == 'c' || c == 's' || c == '[' || c == 'n' || c == '%')
            return p;
        if (is_int_conv(c) || is_float_conv(c))
            return p;
        return NULL;
    }
    if (sp->width_hash)
        return NULL;
    if (is_int_conv(c) || is_float_conv(c) || (c && strchr("cspn%", c)))
        return p;
    if (c == 't' || c == 'T')
        *err = VI_ERROR_NSUP_FMT;               /* %t, %T are for reading */
    return NULL;
}

/* A "\x" sequence of VPP-4.3's format strings (as typed, backslash included): its character, and
 * how far it went. For a format written in a language that does not do escapes itself. */
static int escape_seq(const char *p, int *ch)
{
    switch (p[1]) {
    case 'n': *ch = '\n'; return 2;
    case 'r': *ch = '\r'; return 2;
    case 't': *ch = '\t'; return 2;
    case '\\': *ch = '\\'; return 2;
    case '"': *ch = '"'; return 2;
    default:
        if (p[1] >= '0' && p[1] <= '7') {
            int v = 0, k = 1;
            while (k <= 3 && p[k] >= '0' && p[k] <= '7')
                v = v * 8 + (p[k++] - '0');
            *ch = v & 0xFF;
            return k;
        }
        *ch = '\\';
        return 1;
    }
}

/* ------------------------------------------------------------------------------------------ */
/*  Writing                                                                                   */
/* ------------------------------------------------------------------------------------------ */

typedef struct sink {
    struct fio *f;           /* a session's write buffer, or NULL for memory (viSPrintf) */
    unsigned char *mem;
    size_t n;
    size_t count;            /* bytes produced, for %n */
    int stop;                /* memory: END was produced, the rest of the format is ignored */
} sink;

static ViStatus out(sink *s, const void *p, size_t n)
{
    s->count += n;
    if (s->f)
        return wput(s->f, p, n);
    memcpy(s->mem + s->n, p, n);
    s->n += n;
    return VI_SUCCESS;
}

/* LF with END: the write buffer goes out (RULE 6.2.8: in memory, the format ends there). */
static ViStatus out_end(sink *s)
{
    ViStatus st = out(s, "\n", 1);
    if (st < 0)
        return st;
    if (s->f)
        return wflush(s->f, 1);
    s->stop = 1;
    return VI_SUCCESS;
}

/* A C specifier from a parsed one: flags, width, precision, and the given length and conversion. */
static void c_spec(const spec *sp, char *buf, const char *len, char conv)
{
    char *q = buf;
    *q++ = '%';
    for (int i = 0; i < sp->nflags; i++)
        *q++ = sp->flags[i];
    if (sp->has_width)
        q += sprintf(q, "%d", sp->width < 0 ? 0 : sp->width);
    if (sp->has_prec)
        q += sprintf(q, ".%d", sp->prec < 0 ? 0 : sp->prec);
    while (*len)
        *q++ = *len++;
    *q++ = conv;
    *q = 0;
}

static ViStatus out_printf(sink *s, const char *fmt, ...)
{
    char small[512];
    va_list ap, ap2;
    va_start(ap, fmt);
    va_copy(ap2, ap);
    int n = vsnprintf(small, sizeof small, fmt, ap);
    va_end(ap);
    ViStatus st;
    if (n < 0) {
        st = VI_ERROR_INV_FMT;
    } else if ((size_t)n < sizeof small) {
        st = out(s, small, (size_t)n);
    } else {
        char *big = (char *)malloc((size_t)n + 1);
        if (!big) {
            va_end(ap2);
            return VI_ERROR_ALLOC;
        }
        vsnprintf(big, (size_t)n + 1, fmt, ap2);
        st = out(s, big, (size_t)n);
        free(big);
    }
    va_end(ap2);
    return st;
}

/* #H / #Q / #B and the digits, at least `prec` of them, padded to `width`. */
static ViStatus out_radix(sink *s, const spec *sp, unsigned long long v)
{
    int base = sp->nr == 'H' ? 16 : sp->nr == 'Q' ? 8 : 2;
    char digits[70], field[140];
    int n = 0;
    do {
        digits[n++] = "0123456789ABCDEF"[v % (unsigned)base];
        v /= (unsigned)base;
    } while (v);
    while (sp->has_prec && n < sp->prec && n < 64)
        digits[n++] = '0';
    int k = 0;
    field[k++] = '#';
    field[k++] = sp->nr;
    while (n)
        field[k++] = digits[--n];
    field[k] = 0;
    int left = memchr(sp->flags, '-', (size_t)sp->nflags) != NULL;
    int pad = sp->has_width && sp->width > k ? sp->width - k : 0;
    ViStatus st = VI_SUCCESS;
    for (; !left && pad > 0 && st >= 0; pad--)
        st = out(s, " ", 1);
    if (st >= 0)
        st = out(s, field, (size_t)k);
    for (; left && pad > 0 && st >= 0; pad--)
        st = out(s, " ", 1);
    return st;
}

static ViStatus out_int(sink *s, const spec *sp, long long v, unsigned long long u, int is_unsigned)
{
    char f[64];
    switch (sp->nr) {
    case '2':
        c_spec(sp, f, "", 'f');
        return out_printf(s, f, is_unsigned ? (double)u : (double)v);
    case '3':
        c_spec(sp, f, "", 'E');
        return out_printf(s, f, is_unsigned ? (double)u : (double)v);
    case 'H': case 'Q': case 'B':
        return out_radix(s, sp, is_unsigned ? u : (unsigned long long)v);
    case '1':
        c_spec(sp, f, "ll", 'd');
        return out_printf(s, f, is_unsigned ? (long long)u : v);
    default:
        c_spec(sp, f, "ll", sp->conv);
        return is_unsigned ? out_printf(s, f, u) : out_printf(s, f, v);
    }
}

/* A real number as a long long, truncated: one out of range is the nearest end of it, NaN is 0. A plain
 * cast of those is undefined behaviour in C. */
static long long ll_of(long double v)
{
    if (v != v)
        return 0;
    if (v >= (long double)LLONG_MAX)
        return LLONG_MAX;
    if (v <= (long double)LLONG_MIN)
        return LLONG_MIN;
    return (long long)v;
}

static ViStatus out_float(sink *s, const spec *sp, long double v)
{
    char f[64];
    switch (sp->nr) {
    case '1': {
        spec t = *sp;
        t.has_prec = 0;
        c_spec(&t, f, "ll", 'd');
        return out_printf(s, f, ll_of(v));  /* NR1: truncated */
    }
    case '2':
        c_spec(sp, f, "L", 'f');
        return out_printf(s, f, v);
    case '3':
        c_spec(sp, f, "L", 'E');
        return out_printf(s, f, v);
    case 'H': case 'Q': case 'B':
        return out_radix(s, sp, (unsigned long long)ll_of(v));
    default:
        c_spec(sp, f, "L", sp->conv);
        return out_printf(s, f, v);
    }
}

/* The i-th element of an integer array of the specifier's length. */
static void int_elem(const spec *sp, const void *a, size_t i, long long *v, unsigned long long *u)
{
    int uns = strchr("ouxX", sp->conv) != NULL;
    switch (sp->len) {
    case L_HH: if (uns) *u = ((const unsigned char *)a)[i]; else *v = ((const signed char *)a)[i]; break;
    case L_H:  if (uns) *u = ((const unsigned short *)a)[i]; else *v = ((const short *)a)[i]; break;
    case L_L:  if (uns) *u = ((const unsigned long *)a)[i]; else *v = ((const long *)a)[i]; break;
    case L_LL: if (uns) *u = ((const unsigned long long *)a)[i]; else *v = ((const long long *)a)[i]; break;
    case L_J:  if (uns) *u = ((const uintmax_t *)a)[i]; else *v = ((const intmax_t *)a)[i]; break;
    case L_Z:  *u = ((const size_t *)a)[i]; *v = (long long)*u; break;
    case L_T:  *v = ((const ptrdiff_t *)a)[i]; *u = (unsigned long long)*v; break;
    default:   if (uns) *u = ((const unsigned int *)a)[i]; else *v = ((const int *)a)[i]; break;
    }
}

/* One integer argument of the specifier's length. */
static void int_arg(const spec *sp, va_list *ap, long long *v, unsigned long long *u)
{
    int uns = strchr("ouxX", sp->conv) != NULL;
    switch (sp->len) {
    case L_HH: if (uns) *u = (unsigned char)va_arg(*ap, unsigned int); else *v = (signed char)va_arg(*ap, int); break;
    case L_H:  if (uns) *u = (unsigned short)va_arg(*ap, unsigned int); else *v = (short)va_arg(*ap, int); break;
    case L_L:  if (uns) *u = va_arg(*ap, unsigned long); else *v = va_arg(*ap, long); break;
    case L_LL: if (uns) *u = va_arg(*ap, unsigned long long); else *v = va_arg(*ap, long long); break;
    case L_J:  if (uns) *u = va_arg(*ap, uintmax_t); else *v = va_arg(*ap, intmax_t); break;
    case L_Z:  *u = va_arg(*ap, size_t); *v = (long long)*u; break;
    case L_T:  *v = va_arg(*ap, ptrdiff_t); *u = (unsigned long long)*v; break;
    default:   if (uns) *u = va_arg(*ap, unsigned int); else *v = va_arg(*ap, int); break;
    }
}

static size_t elem_size(const spec *sp)
{
    switch (sp->len) {
    case L_H: return 2;
    case L_L: case L_Z: return 4;
    case L_LL: case L_BIGZ: return 8;
    default: return 1;
    }
}

static int host_little(void)
{
    const uint16_t one = 1;
    return *(const unsigned char *)&one == 1;
}

/* Element bytes in the wire's order: swapped if the host's is not `little`. */
static void order(unsigned char *e, size_t size, int little)
{
    if (size > 1 && host_little() != little) {
        for (size_t i = 0; i < size / 2; i++) {
            unsigned char t = e[i];
            e[i] = e[size - 1 - i];
            e[size - 1 - i] = t;
        }
    }
}

static ViStatus out_elements(sink *s, const unsigned char *data, size_t count, size_t size, int little)
{
    unsigned char chunk[4096];
    size_t per = sizeof chunk / size, i = 0;
    while (i < count) {
        size_t k = count - i < per ? count - i : per;
        memcpy(chunk, data + i * size, k * size);
        for (size_t j = 0; j < k; j++)
            order(chunk + j * size, size, little);
        ViStatus st = out(s, chunk, k * size);
        if (st < 0)
            return st;
        i += k;
    }
    return VI_SUCCESS;
}

/* Format `fmt` into the sink, the arguments taken from `ap`. */
static ViStatus format_into(sink *s, const char *fmt, va_list *ap)
{
    const char *p = fmt;
    while (*p && !s->stop) {
        ViStatus st = VI_SUCCESS;
        if (*p == '\\') {
            int ch, k = escape_seq(p, &ch);
            p += k;
            st = ch == '\n' ? out_end(s) : out(s, &ch, 1);
            if (st < 0)
                return st;
            continue;
        }
        if (*p == '\n') {
            p++;
            if ((st = out_end(s)) < 0)
                return st;
            continue;
        }
        if (*p != '%') {
            const char *q = p;
            while (*q && *q != '%' && *q != '\\' && *q != '\n')
                q++;
            if ((st = out(s, p, (size_t)(q - p))) < 0)
                return st;
            p = q;
            continue;
        }
        spec sp;
        ViStatus err;
        const char *next = parse_spec(p + 1, 0, &sp, &err);
        if (!next)
            return err;
        p = next;
        /* The arguments, in VPP-4.3's order: width, precision, array size, the value. */
        if (sp.width_star) {
            if (is_block_conv(sp.conv))
                sp.width = (int)va_arg(*ap, long);
            else
                sp.width = va_arg(*ap, int);
            if (sp.width < 0 && !is_block_conv(sp.conv)) {
                sp.width = sp.width == INT_MIN ? INT_MAX : -sp.width;
                if (sp.nflags < 7)
                    sp.flags[sp.nflags++] = '-';
            }
        }
        if (sp.prec_star) {
            sp.prec = va_arg(*ap, int);
            if (sp.prec < 0)
                sp.has_prec = 0;
        }
        if (sp.asize_star)
            sp.asize = va_arg(*ap, int);
        char c = sp.conv;
        if (c == '%') {
            st = out(s, "%", 1);
        } else if (c == 'n') {
            void *dst = va_arg(*ap, void *);
            if (dst) {
                switch (sp.len) {
                case L_HH: *(signed char *)dst = (signed char)s->count; break;
                case L_H: *(short *)dst = (short)s->count; break;
                case L_L: *(long *)dst = (long)s->count; break;
                case L_LL: *(long long *)dst = (long long)s->count; break;
                default: *(int *)dst = (int)s->count; break;
                }
            }
        } else if (c == 'c') {
            char f[64];
            c_spec(&sp, f, "", 'c');
            st = out_printf(s, f, va_arg(*ap, int));
        } else if (c == 's') {
            char f[64];
            c_spec(&sp, f, "", 's');
            const char *str = va_arg(*ap, const char *);
            st = out_printf(s, f, str ? str : "");
        } else if (c == 'p') {
            char f[64];
            c_spec(&sp, f, "", 'p');
            st = out_printf(s, f, va_arg(*ap, void *));
        } else if (is_int_conv(c)) {
            int uns = strchr("ouxX", c) != NULL;
            long long v = 0;
            unsigned long long u = 0;
            if (sp.array) {
                const void *a = va_arg(*ap, const void *);
                for (int i = 0; i < sp.asize && st >= 0; i++) {
                    if (i)
                        st = out(s, ",", 1);
                    int_elem(&sp, a, (size_t)i, &v, &u);
                    if (st >= 0)
                        st = out_int(s, &sp, v, u, uns);
                }
            } else {
                int_arg(&sp, ap, &v, &u);
                st = out_int(s, &sp, v, u, uns);
            }
        } else if (is_float_conv(c)) {
            if (sp.array) {
                /* No length: floats. l: doubles. L: long doubles (VPP-4.3's %f table). */
                const void *a = va_arg(*ap, const void *);
                for (int i = 0; i < sp.asize && st >= 0; i++) {
                    if (i)
                        st = out(s, ",", 1);
                    long double v = sp.len == L_L ? ((const double *)a)[i]
                                  : sp.len == L_BIGL ? ((const long double *)a)[i]
                                  : (long double)((const float *)a)[i];
                    if (st >= 0)
                        st = out_float(s, &sp, v);
                }
            } else {
                long double v = sp.len == L_BIGL ? va_arg(*ap, long double) : (long double)va_arg(*ap, double);
                st = out_float(s, &sp, v);
            }
        } else {
            /* %b %B %y: a count of elements, then the data. */
            long count = sp.width;
            const unsigned char *data = va_arg(*ap, const unsigned char *);
            size_t size = elem_size(&sp);
            if (count < 0 || (count > 0 && !data))
                return VI_ERROR_INV_PARAMETER;
            if (c == 'b') {
                unsigned long long bytes = (unsigned long long)count * size;
                char head[24];
                int digits = sprintf(head + 2, "%llu", bytes);
                if (digits > 9)
                    return VI_ERROR_INV_FMT;    /* a definite length has at most nine digits */
                head[0] = '#';
                head[1] = (char)('0' + digits);
                st = out(s, head, (size_t)digits + 2);
            } else if (c == 'B') {
                st = out(s, "#0", 2);
            }
            /* Blocks are in IEEE 488.2 byte order, big-endian; %y as its modifier says. */
            if (st >= 0)
                st = out_elements(s, data, (size_t)count, size, c == 'y' && sp.little);
            if (st >= 0 && c == 'B')
                st = out_end(s);                /* an indefinite block ends with LF and END */
        }
        if (st < 0)
            return st;
    }
    return VI_SUCCESS;
}

static ViStatus check_format(const char *fmt, int reading);

static ViStatus write_formatted(ViSession vi, const char *fmt, va_list *ap)
{
    if (!fmt)
        return VI_ERROR_INV_PARAMETER;
    /* The whole format first: one that fails half-way must not leave half its output buffered. */
    ViStatus bad = check_format(fmt, 0);
    if (bad != VI_SUCCESS)
        return bad;
    ViStatus why;
    struct fio *f = fio_open(vi, &why);
    if (!f)
        return why;
    sink s = {f, NULL, 0, 0, 0};
    ViStatus st = format_into(&s, fmt, ap);
    if (st >= 0 && f->wmode == VI_FLUSH_ON_ACCESS)
        st = wflush(f, 1);
    fio_put(f);
    return st;
}

ViStatus VI_API viVPrintf(ViSession vi, const char *fmt, va_list ap)
{
    symbiotic_op_enter();
    va_list a;
    va_copy(a, ap);
    ViStatus st = write_formatted(vi, fmt, &a);
    va_end(a);
    return symbiotic_op_leave(vi, st, "viVPrintf");
}

ViStatus viPrintf(ViSession vi, const char *fmt, ...)
{
    symbiotic_op_enter();
    va_list ap;
    va_start(ap, fmt);
    ViStatus st = write_formatted(vi, fmt, &ap);
    va_end(ap);
    return symbiotic_op_leave(vi, st, "viPrintf");
}

ViStatus VI_API viVSPrintf(ViSession vi, unsigned char *buf, const char *fmt, va_list ap)
{
    symbiotic_op_enter();
    (void)vi;
    if (!buf || !fmt)
        return symbiotic_op_leave(vi, VI_ERROR_INV_PARAMETER, "viVSPrintf");
    ViStatus bad = check_format(fmt, 0);
    if (bad != VI_SUCCESS)
        return symbiotic_op_leave(vi, bad, "viVSPrintf");
    /* VISA gives no size for this buffer; the caller promises it is big enough. */
    sink s = {NULL, buf, 0, 0, 0};
    va_list a;
    va_copy(a, ap);
    ViStatus st = format_into(&s, fmt, &a);
    va_end(a);
    buf[s.n] = 0;
    return symbiotic_op_leave(vi, st, "viVSPrintf");
}

ViStatus viSPrintf(ViSession vi, unsigned char *buf, const char *fmt, ...)
{
    symbiotic_op_enter();
    va_list ap;
    va_start(ap, fmt);
    ViStatus st = viVSPrintf(vi, buf, fmt, ap);
    va_end(ap);
    return symbiotic_op_leave(vi, st, "viSPrintf");
}

ViStatus VI_API viBufWrite(ViSession vi, const unsigned char *buf, ViUInt32 count, ViUInt32 *ret)
{
    symbiotic_op_enter();
    if (ret)
        *ret = 0;
    if (!buf && count)
        return symbiotic_op_leave(vi, VI_ERROR_INV_PARAMETER, "viBufWrite");
    ViStatus why;
    struct fio *f = fio_open(vi, &why);
    if (!f)
        return symbiotic_op_leave(vi, why, "viBufWrite");
    ViStatus st = wput(f, buf, count);
    if (st >= 0 && f->wmode == VI_FLUSH_ON_ACCESS)
        st = wflush(f, 1);
    fio_put(f);
    if (st >= 0 && ret)
        *ret = count;
    return symbiotic_op_leave(vi, st, "viBufWrite");
}

/* ------------------------------------------------------------------------------------------ */
/*  Reading                                                                                   */
/* ------------------------------------------------------------------------------------------ */

typedef struct src {
    struct fio *f;                  /* a session's read buffer, or NULL for memory (viSScanf) */
    const unsigned char *s;
    size_t pos, len;
    int fresh;                      /* nothing parsed yet: running out may start a new message */
    size_t count;                   /* characters consumed, for %n */
    ViStatus err;                   /* a read failed */
} src;

/* The next character, or -1 where the data ends: the message is over, or a read failed (err). */
static int peekc(src *r)
{
    if (!r->f)
        return r->pos < r->len ? r->s[r->pos] : -1;
    struct fio *f = r->f;
    if (f->rpos < f->rn)
        return f->r[f->rpos];
    if (r->err)
        return -1;
    if (f->r_end) {
        if (!r->fresh)
            return -1;
        f->r_end = 0;                   /* the last message is used up: read the next (6.2.16) */
    }
    ViStatus st = fill(f);
    if (st < 0) {
        r->err = st;
        return -1;
    }
    if (f->rn == 0) {
        r->fresh = 0;                   /* an empty message */
        return -1;
    }
    return f->r[f->rpos];
}

/* Consume the character peekc returned: data (the call has parsed something), or white space. */
static void takec(src *r)
{
    if (r->f)
        r->f->rpos++;
    else
        r->pos++;
    r->count++;
    r->fresh = 0;
}

static void skipc(src *r)
{
    if (r->f)
        r->f->rpos++;
    else
        r->pos++;
    r->count++;
}

static void skip_ws(src *r)
{
    int c;
    while ((c = peekc(r)) >= 0 && isspace(c))
        skipc(r);
}

/* White space at the end of the format: only what is already here, never a read for more. */
static void skip_ws_here(src *r)
{
    if (r->f) {
        while (r->f->rpos < r->f->rn && isspace(r->f->r[r->f->rpos])) {
            r->f->rpos++;
            r->count++;
        }
    } else {
        while (r->pos < r->len && isspace(r->s[r->pos])) {
            r->pos++;
            r->count++;
        }
    }
}

/* Up to n bytes of the current message, into dst (or dropped if dst is NULL): how many. */
static size_t take_bytes(src *r, unsigned char *dst, size_t n)
{
    size_t got = 0;
    while (got < n) {
        if (peekc(r) < 0)
            break;
        size_t avail, k;
        if (r->f) {
            avail = r->f->rn - r->f->rpos;
            k = n - got < avail ? n - got : avail;
            if (dst)
                memcpy(dst + got, r->f->r + r->f->rpos, k);
            r->f->rpos += k;
        } else {
            avail = r->len - r->pos;
            k = n - got < avail ? n - got : avail;
            if (dst)
                memcpy(dst + got, r->s + r->pos, k);
            r->pos += k;
        }
        got += k;
        r->count += k;
        r->fresh = 0;
    }
    return got;
}

/* Would tok[0..n) + c still begin "inf", "infinity" or "nan" (after a sign), case aside? */
static int word_prefix(const char *tok, size_t n, char c)
{
    size_t i = n && (tok[0] == '+' || tok[0] == '-') ? 1 : 0;
    char w[16];
    size_t k = 0;
    for (size_t j = i; j < n && k < sizeof w - 2; j++)
        w[k++] = (char)tolower((unsigned char)tok[j]);
    w[k++] = (char)tolower((unsigned char)c);
    w[k] = 0;
    return strncmp("infinity", w, k) == 0 || strncmp("nan", w, k) == 0;
}

/* A number's characters: NRf (sign, digits, point, exponent), #H #Q #B, and for the C integer
 * conversions their prefixes and digits. At most `width` characters if given. */
static size_t number_token(src *r, char conv, char *tok, size_t cap, int width)
{
    size_t n = 0;
    int c, seen_e = 0, seen_dot = 0, radix = 0;
    int hex = conv == 'x' || conv == 'X' || conv == 'i' || conv == 'a' || conv == 'A';
    while (n + 1 < cap && (width <= 0 || (int)n < width) && (c = peekc(r)) >= 0) {
        int ok = 0;
        char prev = n ? tok[n - 1] : 0;
        if (radix) {
            ok = radix == 16 ? isxdigit(c) : radix == 8 ? (c >= '0' && c <= '7') : (c == '0' || c == '1');
        } else if (n == 1 && tok[0] == '#') {
            if (c == 'H' || c == 'h') radix = 16, ok = 1;
            else if (c == 'Q' || c == 'q') radix = 8, ok = 1;
            else if (c == 'B' || c == 'b') radix = 2, ok = 1;
        } else if (c == '#') {
            ok = n == 0 && conv != 'u' && conv != 'o';
        } else if (c == '+' || c == '-') {
            /* at the start, or opening an exponent */
            ok = n == 0 || ((prev == 'e' || prev == 'E') && seen_e)
                 || ((prev == 'p' || prev == 'P') && (conv == 'a' || conv == 'A'));
        } else if (isdigit(c)) {
            /* Octal (%o, and %i after a leading 0 that is not 0x) ends at an 8 or a 9, as in C: "38" is 3. */
            const char *d = tok + (n && (tok[0] == '+' || tok[0] == '-'));
            size_t nd = (size_t)(tok + n - d);
            int octal = conv == 'o' || (conv == 'i' && nd > 0 && d[0] == '0' && !(nd > 1 && (d[1] == 'x' || d[1] == 'X')));
            ok = !octal || c <= '7';
        } else if (c == '.') {
            ok = !seen_dot && !seen_e && !is_int_conv(conv) ? 1 : (conv == 'd' && !seen_dot && !seen_e);
            seen_dot |= ok;
        } else if ((c == 'e' || c == 'E') && !(hex && is_int_conv(conv))) {
            ok = !seen_e && n > 0 && (isdigit((unsigned char)prev) || prev == '.') && (!is_int_conv(conv) || conv == 'd');
            seen_e |= ok;
        } else if ((c == 'x' || c == 'X') && hex) {
            ok = prev == '0' && (n == 1 || (n == 2 && (tok[0] == '+' || tok[0] == '-')));
        } else if (hex && isxdigit(c)) {
            /* %i is hexadecimal only after 0x: "6a" is 6. */
            const char *d = tok + (n && (tok[0] == '+' || tok[0] == '-'));
            ok = conv != 'i' || (tok + n - d >= 2 && d[0] == '0' && (d[1] == 'x' || d[1] == 'X'));
        } else if ((c == 'p' || c == 'P') && (conv == 'a' || conv == 'A')) {
            ok = n > 0;
        } else if (!is_int_conv(conv) && isalpha(c)) {
            ok = word_prefix(tok, n, (char)c);   /* inf, infinity, nan: the C float conversions */
        }
        if (!ok)
            break;
        tok[n++] = (char)c;
        takec(r);
    }
    tok[n] = 0;
    return n;
}

/* The token as an integer for conversion `conv`; 0 if it is not one. */
static int token_int(const char *tok, char conv, long long *v)
{
    char *end;
    if (tok[0] == '#') {
        int base = tok[1] == 'H' || tok[1] == 'h' ? 16 : tok[1] == 'Q' || tok[1] == 'q' ? 8 : 2;
        if (!tok[2])
            return 0;
        *v = (long long)strtoull(tok + 2, &end, base);
        return *end == 0;
    }
    if (conv == 'd' && strpbrk(tok, ".eE")) {
        double d = strtod(tok, &end);   /* NRf read into an integer: truncated */
        *v = (long long)d;
        return end != tok && *end == 0;
    }
    int base = conv == 'i' ? 0 : conv == 'o' ? 8 : (conv == 'x' || conv == 'X') ? 16 : 10;
    if (conv == 'u' || conv == 'o' || conv == 'x' || conv == 'X')
        *v = (long long)strtoull(tok, &end, base);
    else
        *v = strtoll(tok, &end, base);
    return end != tok && *end == 0;
}

static int token_float(const char *tok, long double *v)
{
    char *end;
    if (tok[0] == '#') {
        long long i;
        if (!token_int(tok, 'd', &i))
            return 0;
        *v = (long double)i;
        return 1;
    }
    *v = (long double)strtod(tok, &end);
    return end != tok && *end == 0;
}

static void store_int(const spec *sp, void *dst, size_t i, long long v)
{
    switch (sp->len) {
    case L_HH: ((signed char *)dst)[i] = (signed char)v; break;
    case L_H: ((short *)dst)[i] = (short)v; break;
    case L_L: ((long *)dst)[i] = (long)v; break;
    case L_LL: ((long long *)dst)[i] = v; break;
    case L_J: ((intmax_t *)dst)[i] = (intmax_t)v; break;
    case L_Z: ((size_t *)dst)[i] = (size_t)v; break;
    case L_T: ((ptrdiff_t *)dst)[i] = (ptrdiff_t)v; break;
    default: ((int *)dst)[i] = (int)v; break;
    }
}

static void store_float(const spec *sp, void *dst, size_t i, long double v)
{
    if (sp->len == L_L)
        ((double *)dst)[i] = (double)v;
    else if (sp->len == L_BIGL)
        ((long double *)dst)[i] = v;
    else
        ((float *)dst)[i] = (float)v;
}

/* One number for a %d or %f family conversion (white space before it skipped): 0 if none. */
static int scan_number(src *r, const spec *sp, void *dst, size_t i)
{
    char tok[128];
    skip_ws(r);
    if (!number_token(r, sp->conv, tok, sizeof tok, sp->array ? 0 : sp->width))
        return 0;
    if (is_int_conv(sp->conv)) {
        long long v;
        if (!token_int(tok, sp->conv, &v))
            return 0;
        if (dst)
            store_int(sp, dst, i, v);
    } else {
        long double v;
        if (!token_float(tok, &v))
            return 0;
        if (dst)
            store_float(sp, dst, i, v);
    }
    return 1;
}

/* Into a string of at most `size` bytes (the NUL included, OBSERVATION 6.2.10: '#'); what does not
 * fit is read and dropped. `max`, when not 0, is a plain width: at most that many characters are
 * read and the rest stays for what follows, as in C (VPP-4.3.6 9.4.13.2: "the maximum number of
 * characters to be parsed"). `stop` says where it ends: 0 white space (%s), 1 END (%t), 2 LF (%T,
 * which keeps it). How many characters were stored. */
static long scan_string(src *r, char *dst, long size, long max, int stop)
{
    long n = 0, taken = 0;
    int c;
    if (stop == 0)
        skip_ws(r);
    while ((max <= 0 || taken < max) && (c = peekc(r)) >= 0) {
        if (stop == 0 && isspace(c))
            break;
        takec(r);
        taken++;
        if (dst && (size <= 0 || n + 1 < size))
            dst[n] = (char)c;
        if (size <= 0 || n + 1 < size)
            n++;
        if (stop == 2 && c == '\n')
            break;
    }
    if (dst)
        dst[n] = 0;
    return n;
}

static int in_set(const spec *sp, int c)
{
    const char *s = sp->set, *e = sp->set + sp->nset;
    int neg = s < e && *s == '^';
    if (neg)
        s++;
    int hit = 0;
    for (const char *q = s; q < e; q++) {
        if (q + 2 < e && q[1] == '-') {          /* a range; a '-' first or last is itself */
            if ((unsigned char)c >= (unsigned char)q[0] && (unsigned char)c <= (unsigned char)q[2])
                hit = 1;
            q += 2;
        } else if ((unsigned char)*q == (unsigned char)c) {
            hit = 1;
        }
    }
    return neg ? !hit : hit;
}

/* An IEEE 488.2 block, definite (#<n><length>) or indefinite (#0, to END; the LF before END is the
 * terminator, not data), into at most `cap` elements of the specifier's size, in IEEE 488.2 byte
 * order. The rest of a block that does not fit is read and dropped. How many elements; -1 if there
 * is no block here. */
static long scan_block(src *r, const spec *sp, unsigned char *dst, long cap)
{
    size_t size = elem_size(sp);
    skip_ws(r);
    if (peekc(r) != '#')
        return -1;
    takec(r);
    int d = peekc(r);
    if (d < '0' || d > '9')
        return -1;
    takec(r);
    unsigned char *all = NULL;
    size_t bytes = 0;
    if (d == '0') {
        size_t allocd = 0;
        for (;;) {
            if (bytes == allocd) {
                allocd = allocd ? allocd * 2 : 4096;
                unsigned char *more = (unsigned char *)realloc(all, allocd);
                if (!more) {
                    free(all);
                    r->err = VI_ERROR_ALLOC;
                    return -1;
                }
                all = more;
            }
            size_t k = take_bytes(r, all + bytes, allocd - bytes);
            bytes += k;
            if (k == 0)
                break;
        }
        if (bytes && all[bytes - 1] == '\n')
            bytes--;
    } else {
        char len[10];
        int k = 0;
        while (k < d - '0') {
            int c = peekc(r);
            if (c < '0' || c > '9')
                return -1;
            takec(r);
            len[k++] = (char)c;
        }
        len[k] = 0;
        size_t want = (size_t)strtoull(len, NULL, 10);
        size_t keep = dst && cap > 0 ? (size_t)cap * size : 0;
        if (keep > want)
            keep = want;
        /* What fits goes straight into the caller's array; the rest is dropped. */
        size_t got = take_bytes(r, dst, keep);
        got += take_bytes(r, NULL, want - keep);
        long n = (long)((got < keep ? got : keep) / size);
        for (long i = 0; dst && i < n; i++)
            order(dst + (size_t)i * size, size, 0);
        return n;
    }
    long n = (long)(bytes / size);
    if (n > cap)
        n = cap;
    if (dst && n > 0) {
        memcpy(dst, all, (size_t)n * size);
        for (long i = 0; i < n; i++)
            order(dst + (size_t)i * size, size, 0);
    }
    free(all);
    return n;
}

/* Parse `fmt` against the source, storing through the arguments in `ap`. */
static ViStatus scan_from(src *r, const char *fmt, va_list *ap)
{
    const char *p = fmt;
    while (*p) {
        int ch = -1, k = 0;
        if (*p == '\\')
            k = escape_seq(p, &ch);
        else if (*p != '%')
            ch = (unsigned char)*p, k = 1;
        if (k) {
            p += k;
            if (isspace(ch)) {
                /* White space matches any amount; at the end of the format, only what is here. */
                const char *q = p;
                while (*q && isspace((unsigned char)*q))
                    q++;
                if (*q)
                    skip_ws(r);
                else
                    skip_ws_here(r);
            } else {
                if (peekc(r) != ch)
                    break;                        /* no match: the rest is not parsed */
                takec(r);
            }
            continue;
        }
        spec sp;
        ViStatus err;
        const char *next = parse_spec(p + 1, 1, &sp, &err);
        if (!next)
            return err;
        p = next;
        char c = sp.conv;
        /* '#': a pointer to the size (in: how many fit; out: how many were stored), then the data. */
        long *hash_long = NULL;
        int *hash_int = NULL;
        if (!sp.suppress && (sp.width_hash || sp.asize_hash)) {
            if (is_block_conv(c))
                hash_long = va_arg(*ap, long *);
            else
                hash_int = va_arg(*ap, int *);
        }
        void *dst = sp.suppress || c == '%' ? NULL : va_arg(*ap, void *);
        long cap = sp.width_hash ? (hash_long ? *hash_long : hash_int ? *hash_int : 0)
                 : sp.asize_hash ? (hash_int ? *hash_int : 0)
                 : sp.array ? sp.asize : sp.width;
        if (c == '%') {
            skip_ws(r);
            if (peekc(r) != '%')
                break;
            takec(r);
        } else if (c == 'n') {
            if (dst)
                store_int(&sp, dst, 0, (long long)r->count);
        } else if (c == 'c') {
            long want = sp.has_width ? cap : 1, n = 0;
            int x;
            while (n < want && (x = peekc(r)) >= 0) {
                takec(r);
                if (dst)
                    ((char *)dst)[n] = (char)x;
                n++;
            }
            if (hash_int)
                *hash_int = (int)n;
            if (n == 0)
                break;
        } else if (c == 's' || c == 't' || c == 'T') {
            int x = c == 's' ? (skip_ws(r), peekc(r)) : peekc(r);
            if (x < 0)
                break;
            long n = scan_string(r, (char *)dst, sp.width_hash ? cap : 0, sp.width_hash ? 0 : sp.has_width ? cap : 0,
                                 c == 's' ? 0 : c == 't' ? 1 : 2);
            if (hash_int)
                *hash_int = (int)n;
        } else if (c == '[') {
            /* With '#' the size is the buffer's, the NUL included, and what does not fit is read and
             * dropped, as %#s; a plain width is how many characters to read, as C's. */
            long n = 0, stored = 0;
            int x;
            while ((!sp.has_width || sp.width_hash || n < cap) && (x = peekc(r)) >= 0 && in_set(&sp, x)) {
                takec(r);
                if (!sp.width_hash || stored + 1 < cap) {
                    if (dst)
                        ((char *)dst)[stored] = (char)x;
                    stored++;
                }
                n++;
            }
            if (hash_int)
                *hash_int = (int)stored;
            if (n == 0)
                break;
            if (dst && (!sp.width_hash || cap > 0))
                ((char *)dst)[stored] = 0;
        } else if (is_int_conv(c) || is_float_conv(c)) {
            if (sp.array) {
                long n = 0;
                while (n < cap) {
                    if (!scan_number(r, &sp, dst, (size_t)n))
                        break;
                    n++;
                    skip_ws_here(r);
                    if (n == cap || peekc(r) != ',')
                        break;
                    takec(r);                     /* the comma between elements */
                }
                if (hash_int)
                    *hash_int = (int)n;
                if (n == 0)
                    break;
            } else if (!scan_number(r, &sp, dst, 0)) {
                break;
            }
        } else if (c == 'b') {
            long n = scan_block(r, &sp, (unsigned char *)dst, dst ? cap : 0);
            if (n < 0)
                break;
            if (hash_long)
                *hash_long = n;
        } else if (c == 'y') {
            size_t size = elem_size(&sp);
            size_t got = take_bytes(r, (unsigned char *)dst, (size_t)(cap > 0 ? cap : 0) * size);
            long n = (long)(got / size);
            for (long i = 0; dst && i < n; i++)
                order((unsigned char *)dst + (size_t)i * size, size, sp.little);
            if (hash_long)
                *hash_long = n;
            if (got == 0)
                break;
        }
        if (r->err)
            break;
    }
    return r->err ? r->err : VI_SUCCESS;
}

/* Check a format without touching arguments or data. */
static ViStatus check_format(const char *fmt, int reading)
{
    for (const char *p = fmt; *p;) {
        if (*p++ != '%')
            continue;
        spec sp;
        ViStatus err;
        const char *next = parse_spec(p, reading, &sp, &err);
        if (!next)
            return err;
        p = next;
    }
    return VI_SUCCESS;
}

static ViStatus read_formatted(ViSession vi, const char *fmt, va_list *ap)
{
    if (!fmt)
        return VI_ERROR_INV_PARAMETER;
    ViStatus st = check_format(fmt, 1);
    if (st != VI_SUCCESS)
        return st;
    ViStatus why;
    struct fio *f = fio_open(vi, &why);
    if (!f)
        return why;
    src r = {f, NULL, 0, 0, 1, 0, VI_SUCCESS};
    st = scan_from(&r, fmt, ap);
    if (f->rmode == VI_FLUSH_ON_ACCESS)
        read_discard(f);
    fio_put(f);
    return st;
}

ViStatus VI_API viVScanf(ViSession vi, const char *fmt, va_list ap)
{
    symbiotic_op_enter();
    va_list a;
    va_copy(a, ap);
    ViStatus st = read_formatted(vi, fmt, &a);
    va_end(a);
    return symbiotic_op_leave(vi, st, "viVScanf");
}

ViStatus viScanf(ViSession vi, const char *fmt, ...)
{
    symbiotic_op_enter();
    va_list ap;
    va_start(ap, fmt);
    ViStatus st = read_formatted(vi, fmt, &ap);
    va_end(ap);
    return symbiotic_op_leave(vi, st, "viScanf");
}

ViStatus VI_API viVSScanf(ViSession vi, const unsigned char *buf, const char *fmt, va_list ap)
{
    symbiotic_op_enter();
    (void)vi;
    if (!buf || !fmt)
        return symbiotic_op_leave(vi, VI_ERROR_INV_PARAMETER, "viVSScanf");
    ViStatus st = check_format(fmt, 1);
    if (st != VI_SUCCESS)
        return symbiotic_op_leave(vi, st, "viVSScanf");
    src r = {NULL, buf, 0, strlen((const char *)buf), 0, 0, VI_SUCCESS};
    va_list a;
    va_copy(a, ap);
    st = scan_from(&r, fmt, &a);
    va_end(a);
    return symbiotic_op_leave(vi, st, "viVSScanf");
}

ViStatus viSScanf(ViSession vi, const unsigned char *buf, const char *fmt, ...)
{
    symbiotic_op_enter();
    va_list ap;
    va_start(ap, fmt);
    ViStatus st = viVSScanf(vi, buf, fmt, ap);
    va_end(ap);
    return symbiotic_op_leave(vi, st, "viSScanf");
}

/* Like viRead, through the read buffer: what viScanf left is read first. */
ViStatus VI_API viBufRead(ViSession vi, unsigned char *buf, ViUInt32 count, ViUInt32 *ret)
{
    symbiotic_op_enter();
    if (ret)
        *ret = 0;
    if (!buf && count)
        return symbiotic_op_leave(vi, VI_ERROR_INV_PARAMETER, "viBufRead");
    ViStatus why;
    struct fio *f = fio_open(vi, &why);
    if (!f)
        return symbiotic_op_leave(vi, why, "viBufRead");
    ViStatus st = VI_SUCCESS;
    ViUInt32 got = 0;
    if (f->rpos == f->rn && f->r_end)
        f->r_end = 0;                         /* the last message is used up: a new one */
    while (got < count) {
        if (f->rpos == f->rn) {
            if (f->r_end)
                break;
            st = fill(f);
            if (st < 0)
                break;
            if (f->rn == 0 && f->r_end)
                break;
        }
        size_t avail = f->rn - f->rpos, k = count - got < avail ? count - got : avail;
        memcpy(buf + got, f->r + f->rpos, k);
        f->rpos += k;
        got += (ViUInt32)k;
    }
    if (st >= 0)
        st = f->rpos == f->rn && f->r_end ? f->r_why : VI_SUCCESS_MAX_CNT;
    if (f->rmode == VI_FLUSH_ON_ACCESS)
        read_discard(f);
    fio_put(f);
    if (ret)
        *ret = got;
    return symbiotic_op_leave(vi, st, "viBufRead");
}

/* ------------------------------------------------------------------------------------------ */
/*  Query                                                                                     */
/* ------------------------------------------------------------------------------------------ */

ViStatus VI_API viVQueryf(ViSession vi, const char *wfmt, const char *rfmt, va_list ap)
{
    symbiotic_op_enter();
    if (!wfmt || !rfmt)
        return symbiotic_op_leave(vi, VI_ERROR_INV_PARAMETER, "viVQueryf");
    ViStatus st = check_format(rfmt, 1);      /* refuse a bad read format before writing */
    if (st != VI_SUCCESS)
        return symbiotic_op_leave(vi, st, "viVQueryf");
    st = check_format(wfmt, 0);
    if (st != VI_SUCCESS)
        return symbiotic_op_leave(vi, st, "viVQueryf");
    ViStatus why;
    struct fio *f = fio_open(vi, &why);
    if (!f)
        return symbiotic_op_leave(vi, why, "viVQueryf");
    /* RULE 6.2.18: the read buffer is flushed before the write, the write buffer after it. */
    read_discard(f);
    va_list a;
    va_copy(a, ap);
    sink s = {f, NULL, 0, 0, 0};
    st = format_into(&s, wfmt, &a);
    if (st >= 0)
        st = wflush(f, 1);
    if (st >= 0) {
        src r = {f, NULL, 0, 0, 1, 0, VI_SUCCESS};
        st = scan_from(&r, rfmt, &a);         /* the rest of the arguments are the read's */
        if (f->rmode == VI_FLUSH_ON_ACCESS)
            read_discard(f);
    }
    va_end(a);
    fio_put(f);
    return symbiotic_op_leave(vi, st, "viVQueryf");
}

ViStatus viQueryf(ViSession vi, const char *wfmt, const char *rfmt, ...)
{
    symbiotic_op_enter();
    va_list ap;
    va_start(ap, rfmt);
    ViStatus st = viVQueryf(vi, wfmt, rfmt, ap);
    va_end(ap);
    return symbiotic_op_leave(vi, st, "viQueryf");
}

/* ------------------------------------------------------------------------------------------ */
/*  For lib.rs: viFlush, viSetBuf, viClose and the buffer attributes                          */
/* ------------------------------------------------------------------------------------------ */

ViStatus symbiotic_fio_flush(ViSession vi, ViUInt16 mask)
{
    if ((mask & VI_READ_BUF) && (mask & VI_READ_BUF_DISCARD))
        return VI_ERROR_INV_MASK;
    if ((mask & VI_WRITE_BUF) && (mask & VI_WRITE_BUF_DISCARD))
        return VI_ERROR_INV_MASK;
    struct fio *f = fio_get(vi, 0);
    if (!f)
        return VI_SUCCESS;                    /* no buffers yet: nothing to do (RULE 6.2.5) */
    ViStatus st = VI_SUCCESS;
    if (mask & VI_WRITE_BUF_DISCARD)
        f->wn = 0;
    if (mask & VI_WRITE_BUF)
        st = wflush(f, 1);
    if (mask & VI_READ_BUF) {
        /* Part of a message is here and the rest is not: read it to its END and drop it, so the
         * next viScanf starts at a message. */
        while (st >= 0 && f->rpos < f->rn && !f->r_end) {
            ViStatus e = fill(f);
            if (e < 0)
                st = e;
        }
        read_discard(f);
    }
    if (mask & VI_READ_BUF_DISCARD)
        read_discard(f);
    fio_put(f);
    return st;
}

ViStatus symbiotic_fio_setbuf(ViSession vi, ViUInt16 mask, ViUInt32 size)
{
    if (size == 0)
        return VI_ERROR_ALLOC;
    ViStatus why;
    struct fio *f = fio_open(vi, &why);
    if (!f)
        return why;
    ViStatus st = VI_SUCCESS;
    if (mask & VI_WRITE_BUF) {
        st = wflush(f, 0);                    /* RULE 6.2.1: an output buffer is flushed first */
        free(f->w);
        f->w = NULL;
        f->wsize = size;
    }
    if (mask & VI_READ_BUF) {
        free(f->r);
        f->r = NULL;
        f->rsize = size;
        read_discard(f);
    }
    fio_put(f);
    return st;
}

void symbiotic_fio_close(ViSession vi)
{
    LOCK(&table_lock);
    struct fio **pp = &table;
    while (*pp && (*pp)->vi != vi)
        pp = &(*pp)->next;
    struct fio *f = *pp;
    if (f)
        *pp = f->next;
    UNLOCK(&table_lock);
    if (f) {
        free(f->w);
        free(f->r);
        free(f);
    }
}

/* VI_ATTR_RD/WR_BUF_OPER_MODE and _SIZE. 1: not one of these (the caller answers it). */
ViStatus symbiotic_fio_attr(ViSession vi, ViAttr attr, int set, uint64_t *value)
{
    if (attr != VI_ATTR_RD_BUF_OPER_MODE && attr != VI_ATTR_WR_BUF_OPER_MODE && attr != VI_ATTR_RD_BUF_SIZE
        && attr != VI_ATTR_WR_BUF_SIZE)
        return 1;
    ViStatus why;
    struct fio *f = fio_open(vi, &why);
    if (!f)
        return why;
    ViStatus st = VI_SUCCESS;
    if (!set) {
        *value = attr == VI_ATTR_RD_BUF_OPER_MODE ? (ViAttrState)f->rmode
               : attr == VI_ATTR_WR_BUF_OPER_MODE ? (ViAttrState)f->wmode
               : attr == VI_ATTR_RD_BUF_SIZE ? (ViAttrState)f->rsize : (ViAttrState)f->wsize;
    } else if (attr == VI_ATTR_RD_BUF_OPER_MODE) {
        if (*value == VI_FLUSH_ON_ACCESS || *value == VI_FLUSH_DISABLE)
            f->rmode = (int)*value;
        else
            st = VI_ERROR_NSUP_ATTR_STATE;
    } else if (attr == VI_ATTR_WR_BUF_OPER_MODE) {
        if (*value == VI_FLUSH_ON_ACCESS || *value == VI_FLUSH_WHEN_FULL)
            f->wmode = (int)*value;
        else
            st = VI_ERROR_NSUP_ATTR_STATE;
    } else {
        st = VI_ERROR_ATTR_READONLY;          /* the sizes are viSetBuf's */
    }
    fio_put(f);
    return st;
}
