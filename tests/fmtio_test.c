/*
 * fmtio_test.c -- crates/symbiotic-visa/src/fmtio.c against VPP-4.3's formatted I/O rules, with no
 * DLL and no hardware: viWrite, viRead and the attribute calls it makes are a fake device here.
 *     tools/check.sh builds and runs it (clang, the host's C runtime).
 * The fake device hands out scripted messages -- each ends with END -- a read at a time, and records
 * every write with whether it ended with END (VI_ATTR_SEND_END_EN when the write was made).
 */
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef uint32_t ViSession;
typedef int32_t ViStatus;
typedef uint32_t ViUInt32;
typedef uint16_t ViUInt16;
typedef uint64_t ViAttrState;

#define VI_SUCCESS            0
#define VI_SUCCESS_MAX_CNT    0x3FFF0006
#define VI_ERROR_TMO          ((ViStatus)0xBFFF0015)
#define VI_ERROR_INV_FMT      ((ViStatus)0xBFFF003F)
#define VI_ERROR_NSUP_FMT     ((ViStatus)0xBFFF0041)
#define VI_ERROR_INV_MASK     ((ViStatus)0xBFFF003D)
#define VI_ATTR_SEND_END_EN   0x3FFF0016u
#define VI_ATTR_RD_BUF_OPER_MODE 0x3FFF002Au
#define VI_READ_BUF 1
#define VI_WRITE_BUF 2
#define VI_READ_BUF_DISCARD 4

ViStatus viPrintf(ViSession, const char *, ...);
ViStatus viSPrintf(ViSession, unsigned char *, const char *, ...);
ViStatus viScanf(ViSession, const char *, ...);
ViStatus viSScanf(ViSession, const unsigned char *, const char *, ...);
ViStatus viQueryf(ViSession, const char *, const char *, ...);
ViStatus viBufRead(ViSession, unsigned char *, ViUInt32, ViUInt32 *);
ViStatus viBufWrite(ViSession, const unsigned char *, ViUInt32, ViUInt32 *);
ViStatus symbiotic_fio_flush(ViSession, ViUInt16);
ViStatus symbiotic_fio_setbuf(ViSession, ViUInt16, ViUInt32);
void symbiotic_fio_close(ViSession);
ViStatus symbiotic_fio_attr(ViSession, uint32_t, int, ViAttrState *);

/* ---- the fake device ------------------------------------------------------------------------ */

static unsigned char out[65536];
static size_t out_n;
static int writes, ended_writes, send_end = 1;
static size_t last_write_len;

static const char *msgs[16];
static size_t msg_len[16];
static int nmsg, cur;
static size_t cur_pos;
static int reads;

ViStatus viWrite(ViSession vi, const unsigned char *buf, ViUInt32 count, ViUInt32 *ret)
{
    (void)vi;
    memcpy(out + out_n, buf, count);
    out_n += count;
    writes++;
    ended_writes += send_end;
    last_write_len = count;
    if (ret)
        *ret = count;
    return VI_SUCCESS;
}

ViStatus viRead(ViSession vi, unsigned char *buf, ViUInt32 count, ViUInt32 *ret)
{
    (void)vi;
    reads++;
    *ret = 0;
    if (cur >= nmsg)
        return VI_ERROR_TMO;
    size_t left = msg_len[cur] - cur_pos, k = count < left ? count : left;
    memcpy(buf, msgs[cur] + cur_pos, k);
    cur_pos += k;
    *ret = (ViUInt32)k;
    if (cur_pos == msg_len[cur]) {
        cur++;
        cur_pos = 0;
        return VI_SUCCESS;                  /* END */
    }
    return VI_SUCCESS_MAX_CNT;
}

/* A session number the fake device does not have. */
#define BAD_VI 0xDEAD

ViStatus viGetAttribute(ViSession vi, uint32_t attr, void *v)
{
    if (vi == BAD_VI)
        return (ViStatus)0xBFFF000E;        /* VI_ERROR_INV_OBJECT */
    if (attr == VI_ATTR_SEND_END_EN) {
        *(ViUInt16 *)v = (ViUInt16)send_end;
        return VI_SUCCESS;
    }
    return -1;
}

ViStatus viSetAttribute(ViSession vi, uint32_t attr, ViAttrState v)
{
    (void)vi;
    if (attr == VI_ATTR_SEND_END_EN)
        send_end = v != 0;
    return VI_SUCCESS;
}

/* lib.rs brackets the exported functions for its exception event; nothing to do here. */
void symbiotic_op_enter(void) {}
ViStatus symbiotic_op_leave(ViSession v, ViStatus st, const char *oper)
{
    (void)v;
    (void)oper;
    return st;
}

static ViSession vi = 1;

/* A fresh session and device, with these messages to read (each ends with END). */
static void reset(int n, ...)
{
    symbiotic_fio_close(vi);
    vi++;
    out_n = 0;
    writes = ended_writes = reads = 0;
    send_end = 1;
    va_list ap;
    va_start(ap, n);
    for (int i = 0; i < n; i++) {
        msgs[i] = va_arg(ap, const char *);
        msg_len[i] = va_arg(ap, size_t);
    }
    va_end(ap);
    nmsg = n;
    cur = 0;
    cur_pos = 0;
}

#define M(s) s, sizeof(s) - 1

static int fails;
static void check(const char *what, int ok)
{
    printf("  %-4s %s\n", ok ? "PASS" : "FAIL", what);
    fails += !ok;
}

static int sent(const void *want, size_t n)
{
    return out_n == n && memcmp(out, want, n) == 0;
}

int main(void)
{
    ViStatus st;

    printf("-- a session that does not exist\n");
    {
        char t[16];
        ViUInt32 n = 0;
        reset(0);
        check("viPrintf refuses it, as NI-VISA does (it used to buffer the text and succeed)",
              viPrintf(BAD_VI, "VOLT %d", 3) == (ViStatus)0xBFFF000E && writes == 0);
        check("viBufWrite, viScanf, viQueryf likewise",
              viBufWrite(BAD_VI, (const unsigned char *)"x", 1, &n) == (ViStatus)0xBFFF000E
              && viScanf(BAD_VI, "%t", t) == (ViStatus)0xBFFF000E
              && viQueryf(BAD_VI, "*IDN?\n", "%t", t) == (ViStatus)0xBFFF000E && writes == 0);
    }

    printf("-- writing\n");
    reset(0);
    st = viPrintf(vi, "%d,%s\n", 5, "abc");
    check("one write, with END, on \"\\n\"", st == 0 && sent("5,abc\n", 6) && writes == 1 && ended_writes == 1);
    reset(0);
    viPrintf(vi, "VOLT %d", 3);
    check("no \"\\n\": nothing sent yet", writes == 0);
    viPrintf(vi, ";CURR %d\n", 4);
    check("...and the next \"\\n\" sends both, as one message", sent("VOLT 3;CURR 4\n", 14) && writes == 1);
    reset(0);
    viPrintf(vi, "*RST");
    symbiotic_fio_flush(vi, VI_WRITE_BUF);
    check("viFlush(VI_WRITE_BUF) sends it, with END", sent("*RST", 4) && ended_writes == 1);
    reset(0);
    symbiotic_fio_setbuf(vi, VI_WRITE_BUF, 8);
    viPrintf(vi, "0123456789ABCDEFGHIJ\n");
    check("a full buffer goes out without END, the rest with it",
          sent("0123456789ABCDEFGHIJ\n", 21) && writes == 3 && ended_writes == 1 && last_write_len == 5);
    check("...and SEND_END_EN is back on", send_end == 1);

    reset(0);
    unsigned char bytes[5] = {1, 2, 3, 10, 0};
    viPrintf(vi, "DATA %5b\n", bytes);
    check("%5b: a definite block", sent("DATA #15\x01\x02\x03\x0A\x00\n", 14));
    reset(0);
    uint16_t words[3] = {0x0102, 0x0304, 0xA0B0};
    viPrintf(vi, "%*hb\n", 3L, words);
    check("%*hb: count as an argument, 16-bit big-endian", sent("#16\x01\x02\x03\x04\xA0\xB0\n", 10));
    reset(0);
    double d2[1] = {1.0};
    viPrintf(vi, "%1Zb", d2);
    symbiotic_fio_flush(vi, VI_WRITE_BUF);
    check("%Zb: doubles, IEEE 754 big-endian", sent("#18\x3F\xF0\0\0\0\0\0\0", 11));
    reset(0);
    viPrintf(vi, "%3B", "xyz");
    check("%B: an indefinite block, LF and END after it", sent("#0xyz\n", 6) && ended_writes == 1);
    reset(0);
    uint16_t le[2] = {0x0102, 0x0304};
    viPrintf(vi, "%2!olhy\n", le);
    check("%!olhy: raw, little-endian", sent("\x02\x01\x04\x03\n", 5));
    reset(0);
    viPrintf(vi, "%2hy\n", le);
    check("%hy: raw, big-endian by default", sent("\x01\x02\x03\x04\n", 5));

    reset(0);
    int ia[3] = {1, -2, 3};
    double da[2] = {0.5, 1.25};
    float fa[2] = {2.5f, -1.0f};
    viPrintf(vi, "%,3d;%,2lf;%.1,*f\n", ia, da, 2, fa);
    check("arrays: %,3d %,2lf and %.1,*f (floats without l)", sent("1,-2,3;0.500000,1.250000;2.5,-1.0\n", 34));
    reset(0);
    st = viPrintf(vi, "OK %d %q\n", 1);
    check("a bad format is refused before anything goes into the buffer", st == VI_ERROR_INV_FMT && writes == 0);
    viPrintf(vi, "X\n");
    check("...so nothing of it follows the next write", sent("X\n", 2));
    reset(0);
    viPrintf(vi, "%@1f %@2d %@3d %@Hd %@Bd %@Q.4d\n", 12.7, 5, 123, 255, 5, 8);
    check("@1 @2 @3 @H @B @Q", sent("12 5.000000 1.230000E+02 #HFF #B101 #Q0010\n", 43));
    reset(0);
    viPrintf(vi, "%@1f %@1f %@1f\n", 1e300, -1e300, __builtin_nan(""));
    check("@1 of a number out of range is the range's end, of NaN 0 (not C's undefined cast)",
          sent("9223372036854775807 -9223372036854775808 0\n", 43));
    reset(0);
    viPrintf(vi, "%-6s|%05.1f|%x|%c|%%|%lld\n", "ab", 3.14159, 255u, 'Z', 1LL << 40);
    check("C conversions pass through", sent("ab    |003.1|ff|Z|%|1099511627776\n", 34));
    reset(0);
    viPrintf(vi, "A\\tB\\n");
    check("\"\\\\t\" and \"\\\\n\" typed out: TAB, and LF with END", sent("A\tB\n", 4) && ended_writes == 1);

    unsigned char sbuf[64];
    memset(sbuf, 'x', sizeof sbuf);
    viSPrintf(vi, sbuf, "a%d\nb%d", 1, 2);
    check("viSPrintf: the format ends at \"\\n\" (RULE 6.2.8)", strcmp((char *)sbuf, "a1\n") == 0);
    viSPrintf(vi, sbuf, "%.99999999999s|", "abc");
    check("a precision too large for an int is the largest, not an overflow", strcmp((char *)sbuf, "abc|") == 0);

    check("%t is refused in a write format", viPrintf(vi, "%t", "x") == VI_ERROR_NSUP_FMT);
    check("%Zd is not a format", viPrintf(vi, "%Zd", 1) == VI_ERROR_INV_FMT);
    check("a block needs its count", viPrintf(vi, "%b", "x") == VI_ERROR_INV_FMT);

    printf("-- reading\n");
    double x[3] = {0};
    reset(1, M("1.5,2.5,3.5\n"));
    st = viScanf(vi, "%,3lf", x);
    check("%,3lf: three doubles", st == 0 && x[0] == 1.5 && x[1] == 2.5 && x[2] == 3.5);

    int a = 0, b = -1;
    reset(2, M("12 34\n"), M("56\n"));
    viScanf(vi, "%d", &a);
    check("a call parses what it needs", a == 12 && reads == 1);
    viScanf(vi, "%d", &a);
    check("...and the next the rest, without reading again", a == 34 && reads == 1);
    viScanf(vi, "%d", &a);
    check("only white space left: it is skipped and a new message read (OBS 6.2.13)", a == 56 && reads == 2);

    reset(2, M("7\n"), M("8\n"));
    a = 0;
    st = viScanf(vi, "%d%d", &a, &b);
    check("the message ends: the second conversion is skipped, success (OBS 6.2.8)", st == 0 && a == 7 && b == -1 && reads == 1);

    char s[32];
    reset(1, M("hello world\n"));
    viScanf(vi, "%t", s);
    check("%t: to END, the last character included", strcmp(s, "hello world\n") == 0);
    reset(1, M("line one\nline two\n"));
    viScanf(vi, "%T", s);
    check("%T: to LF, included", strcmp(s, "line one\n") == 0);
    viScanf(vi, "%T", s);
    check("...and the next line from what was left", strcmp(s, "line two\n") == 0 && reads == 1);

    int cap = 4;
    reset(1, M("longword next\n"));
    viScanf(vi, "%#s %s", &cap, s, s + 10);
    check("%#s: the size (NUL included) in, the count out; the rest of the word dropped",
          strcmp(s, "lon") == 0 && cap == 3 && strcmp(s + 10, "next") == 0);
    reset(1, M("longword next\n"));
    viScanf(vi, "%4s%s", s, s + 10);
    check("%4s: a plain width is how many characters, as C's; the rest stays for the next (VPP-4.3.6 9.4.13.2)",
          strcmp(s, "long") == 0 && strcmp(s + 10, "word") == 0);

    char *small = malloc(4);
    cap = 4;
    reset(1, M("ZZZZZZ,1\n"));
    viScanf(vi, "%#[Z],%d", &cap, small, &a);
    check("%#[: as %#s, the size (NUL included) in, the count out, the rest dropped (no byte past the buffer)",
          strcmp(small, "ZZZ") == 0 && cap == 3 && a == 1);
    free(small);
    reset(1, M("12.5V\n"));
    viScanf(vi, "%[0-9.]%s", s, s + 10);
    check("%[0-9.]: a range first in the set is a range", strcmp(s, "12.5") == 0 && strcmp(s + 10, "V") == 0);
    {
        int o, d, i1, i2, i3, i4, d2;
        reset(1, M("38 6a 0x1f 017 019\n"));
        viScanf(vi, "%o%d %i%s %i %i %i%d", &o, &d, &i1, s, &i2, &i3, &i4, &d2);
        check("octal ends at an 8 or a 9 and %i is hexadecimal only after 0x, as in C",
              o == 3 && d == 8 && i1 == 6 && strcmp(s, "a") == 0 && i2 == 31 && i3 == 15 && i4 == 1 && d2 == 9);
    }
    reset(1, M("a-b+\n"));
    viScanf(vi, "%[-a-z]", s);
    check("%[-a-z]: a '-' first is itself", strcmp(s, "a-b") == 0);

    unsigned char blk[16];
    long n = 16;
    reset(1, M("#210ABCDEFGHIJ\n"));
    st = viScanf(vi, "%#b", &n, blk);
    check("%#b: a definite block, count returned", st == 0 && n == 10 && memcmp(blk, "ABCDEFGHIJ", 10) == 0);
    n = 4;
    reset(1, M("#210ABCDEFGHIJ,42\n"));
    viScanf(vi, "%#b,%d", &n, blk, &a);
    check("a block that does not fit: what fits, the rest dropped, parsing goes on", n == 4 && memcmp(blk, "ABCD", 4) == 0 && a == 42);
    n = 16;
    reset(1, M("#0abc\n"));
    viScanf(vi, "%#b", &n, blk);
    check("an indefinite block: to END, the final LF not data", n == 3 && memcmp(blk, "abc", 3) == 0);
    float fl[2];
    n = 2;
    reset(1, M("#18\x3F\x80\0\0\xC0\0\0\0\n"));
    viScanf(vi, "%#zb", &n, fl);
    check("%zb: big-endian IEEE 754 floats", n == 2 && fl[0] == 1.0f && fl[1] == -2.0f);
    uint16_t hw[2];
    n = 2;
    reset(1, M("\x02\x01\x04\x03"));
    viScanf(vi, "%#!olhy", &n, hw);
    check("%!olhy: raw little-endian words", n == 2 && hw[0] == 0x0102 && hw[1] == 0x0304);

    reset(1, M("#HFF,1.5E2,#B101,-7\n"));
    int h = 0, e = 0, bb = 0, m = 0;
    viScanf(vi, "%d,%d,%d,%d", &h, &e, &bb, &m);
    check("numbers: #H, NRf into an integer, #B, a sign", h == 255 && e == 150 && bb == 5 && m == -7);
    short sh = 0;
    long long ll = 0;
    double dd = 0;
    reset(1, M("9 12345678901 2.5e-3\n"));
    viScanf(vi, "%hd %lld %lf", &sh, &ll, &dd);
    check("lengths: h, ll, l", sh == 9 && ll == 12345678901LL && dd == 2.5e-3);
    reset(1, M("3A\n"));
    float f1 = 0;
    char unit[4];
    viScanf(vi, "%f%s", &f1, unit);
    check("a number followed by letters (\"3A\"): the number, then the rest", f1 == 3.0f && strcmp(unit, "A") == 0);

    reset(1, M("x,y\n"));
    char c2[3] = {0};
    viScanf(vi, "%c,%c", &c2[0], &c2[1]);
    check("%c and a literal", c2[0] == 'x' && c2[1] == 'y');

    printf("-- the buffer across reads\n");
    reset(1, M("#216abcdefghijklmnop,123456,end\n"));
    symbiotic_fio_setbuf(vi, VI_READ_BUF, 4);
    n = 16;
    viScanf(vi, "%#b,%d,%s", &n, blk, &a, s);
    check("4-byte reads: a block, a number and a word across them", n == 16 && memcmp(blk, "abcdefghijklmnop", 16) == 0
          && a == 123456 && strcmp(s, "end") == 0 && reads > 5);

    reset(0);
    st = viScanf(vi, "%d", &a);
    check("nothing comes: the timeout is the answer", st == VI_ERROR_TMO);

    printf("-- query, flush, buffered read\n");
    reset(2, M("stale 1\n"), M("99\n"));
    viScanf(vi, "%s", s);
    st = viQueryf(vi, "READ?\n", "%d", &a);
    check("viQueryf drops what was left before it writes (RULE 6.2.18)", st == 0 && a == 99 && sent("READ?\n", 6));
    reset(1, M("5\n"));
    st = viQueryf(vi, "MEAS?", "%d", &a);
    check("viQueryf flushes its write even without \"\\n\"", st == 0 && a == 5 && writes == 1 && ended_writes == 1);

    unsigned char rb[32];
    ViUInt32 got = 0;
    reset(1, M("12,rest\n"));
    viScanf(vi, "%d", &a);
    st = viBufRead(vi, rb, sizeof rb, &got);
    check("viBufRead: what viScanf left, ending as the message did", st == 0 && got == 6 && memcmp(rb, ",rest\n", 6) == 0);
    reset(0);
    viBufWrite(vi, (const unsigned char *)"*CLS", 4, &got);
    check("viBufWrite: into the write buffer", writes == 0 && got == 4);
    viPrintf(vi, "\n");
    check("...sent with the next \"\\n\"", sent("*CLS\n", 5));

    reset(1, M("12 34\n"));
    viScanf(vi, "%d", &a);
    ViAttrState mode = 1;                       /* VI_FLUSH_ON_ACCESS */
    symbiotic_fio_attr(vi, VI_ATTR_RD_BUF_OPER_MODE, 1, &mode);
    symbiotic_fio_flush(vi, VI_READ_BUF_DISCARD);
    st = viScanf(vi, "%d", &a);
    check("viFlush(VI_READ_BUF_DISCARD) drops what was left", st == VI_ERROR_TMO);
    check("both flushes of one buffer at once are refused", symbiotic_fio_flush(vi, VI_READ_BUF | VI_READ_BUF_DISCARD) == VI_ERROR_INV_MASK);

    a = b = 0;
    viSScanf(vi, (const unsigned char *)"10,20", "%d,%d", &a, &b);
    check("viSScanf from a string", a == 10 && b == 20);

    printf("\n%s: %d failure(s)\n", fails ? "FAIL" : "PASS", fails);
    return fails != 0;
}
