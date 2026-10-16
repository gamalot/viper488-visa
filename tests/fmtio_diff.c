/*
 * fmtio_diff.c -- viSScanf (crates/symbiotic-visa/src/fmtio.c) against the C library's sscanf, on
 * random formats and inputs made of the conversions whose meaning VPP-4.3 takes from C: %d %u %x %o %i
 * %s %c %[set] %[^set], with and without a width, and literal characters. Each case is run by both;
 * the values a conversion stored must agree. How VISA differs from C on purpose (a number read as
 * IEEE 488.2 with rounding, %t, %b, '#', arrays) is not made here.
 *     tools/check.sh builds and runs it (clang, the host's C runtime, ASan and UBSan).
 *     fmtio_diff [cases] [seed]
 */
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef uint32_t ViSession;
typedef int32_t ViStatus;
typedef uint32_t ViUInt32;
typedef uint64_t ViAttrState;

ViStatus viSScanf(ViSession, const unsigned char *, const char *, ...);

/* What fmtio.c calls besides: viSScanf reads no device, so none of it is reached. */
ViStatus viWrite(ViSession vi, const unsigned char *b, ViUInt32 n, ViUInt32 *ret)
{
    (void)vi, (void)b, (void)n, (void)ret;
    return -1;
}
ViStatus viRead(ViSession vi, unsigned char *b, ViUInt32 n, ViUInt32 *ret)
{
    (void)vi, (void)b, (void)n;
    *ret = 0;
    return -1;
}
ViStatus viGetAttribute(ViSession vi, uint32_t a, void *v)
{
    (void)vi, (void)a, (void)v;
    return -1;
}
ViStatus viSetAttribute(ViSession vi, uint32_t a, ViAttrState v)
{
    (void)vi, (void)a, (void)v;
    return 0;
}
void symbiotic_op_enter(void) {}
ViStatus symbiotic_op_leave(ViSession v, ViStatus st, const char *oper)
{
    (void)v, (void)oper;
    return st;
}

/* ---- the cases ------------------------------------------------------------------------------- */

static uint64_t rng;
static unsigned next(unsigned n)
{
    rng = rng * 6364136223846793005ull + 1442695040888963407ull;
    return (unsigned)((rng >> 33) % n);
}

enum kind { K_INT, K_STR, K_CHARS };
#define MAXC 4
#define SLOT 64

typedef struct {
    char fmt[128];
    enum kind kind[MAXC];
    int width[MAXC];
    int n;
} fcase;

static void make(fcase *f)
{
    static const char *ints[] = { "d", "u", "x", "o", "i" };
    static const char *sets[] = { "[a-c]", "[0-9]", "[^ ]", "[^,]", "[-a]", "[0-9a-f]", "[^0-9]" };
    static const char *lits[] = { " ", ",", ":", "" };
    char *p = f->fmt;
    f->n = 1 + (int)next(MAXC);
    for (int i = 0; i < f->n; i++) {
        int w = next(3) == 0 ? 1 + (int)next(6) : 0;
        f->width[i] = w;
        char ws[8] = "";
        if (w)
            snprintf(ws, sizeof ws, "%d", w);
        switch (next(4)) {
        case 0:
        case 1:
            f->kind[i] = K_INT;
            p += sprintf(p, "%%%s%s", ws, ints[next(5)]);
            break;
        case 2:
            f->kind[i] = K_STR;
            p += sprintf(p, "%%%s%s", ws, next(2) ? "s" : sets[next(7)]);
            break;
        default:
            /* %c with no width is one character; with one, that many and no NUL: both compared by count. */
            f->kind[i] = K_CHARS;
            if (!w)
                f->width[i] = 1;
            p += sprintf(p, "%%%sc", ws);
            break;
        }
        p += sprintf(p, "%s", lits[next(4)]);
    }
}

static void input(char *s, size_t cap)
{
    static const char alpha[] = "0123456789abcfxyz -+,:\t";
    size_t n = next((unsigned)cap - 1);
    for (size_t i = 0; i < n; i++)
        s[i] = alpha[next(sizeof alpha - 1)];
    s[n] = 0;
}

/* The values in `a` and `b` for the first `count` conversions. */
static int same(const fcase *f, int count, int ai[], int bi[], char as[][SLOT], char bs[][SLOT])
{
    for (int i = 0; i < count; i++) {
        if (f->kind[i] == K_INT && ai[i] != bi[i])
            return 0;
        if (f->kind[i] == K_STR && strcmp(as[i], bs[i]) != 0)
            return 0;
        if (f->kind[i] == K_CHARS && memcmp(as[i], bs[i], (size_t)f->width[i]) != 0)
            return 0;
    }
    return 1;
}

#define ARGS(v, s) \
    (f.kind[0] == K_INT ? (void *)&v[0] : (void *)s[0]), (f.kind[1] == K_INT ? (void *)&v[1] : (void *)s[1]), \
    (f.kind[2] == K_INT ? (void *)&v[2] : (void *)s[2]), (f.kind[3] == K_INT ? (void *)&v[3] : (void *)s[3])

int main(int argc, char **argv)
{
    long cases = argc > 1 ? atol(argv[1]) : 200000;
    rng = argc > 2 ? strtoull(argv[2], NULL, 0) : 1;
    long bad = 0;
    for (long k = 0; k < cases; k++) {
        fcase f;
        char in[24];
        make(&f);
        input(in, sizeof in);
        int ci[MAXC], vi_[MAXC];
        char cs[MAXC][SLOT], vs[MAXC][SLOT];
        for (int i = 0; i < MAXC; i++) {
            ci[i] = vi_[i] = 0x5A5A5A5A;
            memset(cs[i], 0x5A, SLOT);
            memset(vs[i], 0x5A, SLOT);
        }
        int got = sscanf(in, f.fmt, ARGS(ci, cs));
        if (got < 0)
            got = 0;
        viSScanf(1, (const unsigned char *)in, f.fmt, ARGS(vi_, vs));
        if (!same(&f, got, ci, vi_, cs, vs)) {
            if (bad++ < 20) {
                printf("  DIFF format \"%s\" input \"%s\": sscanf stored %d;", f.fmt, in, got);
                for (int i = 0; i < got; i++) {
                    if (f.kind[i] == K_INT)
                        printf(" [%d] %d vs %d", i, ci[i], vi_[i]);
                    else if (f.kind[i] == K_STR)
                        printf(" [%d] \"%s\" vs \"%.*s\"", i, cs[i], SLOT - 1, vs[i]);
                    else
                        printf(" [%d] \"%.*s\" vs \"%.*s\"", i, f.width[i], cs[i], f.width[i], vs[i]);
                }
                printf("\n");
            }
        }
    }
    printf("%s: %ld case(s), %ld difference(s)\n", bad ? "FAIL" : "PASS", cases, bad);
    return bad != 0;
}
