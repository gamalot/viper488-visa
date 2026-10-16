/*
 * c_api.c -- Symbiotic's C API from C: include/visa.h, linked against visa64.lib, on hardware.
 *     c_api.exe GPIB1::20::INSTR [USB0::0x0957::0x1792::MY20000003::INSTR]
 * Built by tools/build_c_api.sh (from WSL, with the cargo-xwin SDK); run it on Windows with
 * visa64.dll and symbioticd.exe beside it, or with them on the PATH.
 *
 * The first resource is any SCPI instrument (only *IDN?, *OPC? and SYST:ERR? are sent). It checks
 * what the header promises and pyvisa never calls: viReadAsync / viWriteAsync with the I/O
 * completion event and its attributes, viTerminate, viReadToFile / viWriteFromFile, viBufWrite /
 * viBufRead, formatted I/O, an operation this runtime does not do (viIn8), and the second
 * resource, if given, a USBTMC instrument, with viUsbControlIn (GET_STATUS to the device).
 *
 * VISA's own formatted I/O: %t on any instrument; on an Agilent 33210A / 33220A, a 1000-point
 * waveform sent with %*hb into its volatile memory (it takes big-endian 16-bit blocks, as IEEE 488.2
 * orders them; nothing is output unless FUNC USER is selected, which this does not do); on a Keysight
 * DSO-X oscilloscope (the second resource), the waveform preamble read with %,#lf and the data
 * with %#b.
 */
#include <stdio.h>
#include <string.h>
#include "visa.h"

static int fails;

static void check(const char *what, int ok, ViStatus st)
{
    printf("  %-4s %s   [status 0x%08lX]\n", ok ? "PASS" : "FAIL", what, (unsigned long)(ViUInt32)st);
    fails += !ok;
}

int main(int argc, char **argv)
{
    ViSession rm, vi, ev;
    ViStatus st;
    ViChar buf[512], name[256];
    ViUInt32 n = 0;
    ViJobId job = 0;
    ViEventType ty = 0;

    if (argc < 2) {
        fprintf(stderr, "usage: c_api RESOURCE [USBTMC-RESOURCE]\n");
        return 2;
    }
    st = viOpenDefaultRM(&rm);
    check("viOpenDefaultRM", st == VI_SUCCESS, st);
    viGetAttribute(rm, VI_ATTR_RSRC_MANF_NAME, name);
    printf("  (resource manager: %s)\n", name);
    st = viOpen(rm, argv[1], VI_NULL, 2000, &vi);
    check("viOpen", st == VI_SUCCESS, st);
    if (st != VI_SUCCESS)
        return 1;
    viSetAttribute(vi, VI_ATTR_TMO_VALUE, 3000);
    viSetAttribute(vi, VI_ATTR_TERMCHAR_EN, VI_TRUE);
    viWrite(vi, (ViConstBuf)"*CLS\n", 5, &n);         /* an earlier run's errors are not this one's */

    /* Formatted I/O, as a C program usually talks. */
    st = viQueryf(vi, "*IDN?\n", "%255[^\n]", buf);
    check("viQueryf *IDN?", st == VI_SUCCESS && strchr(buf, ',') != NULL, st);
    printf("  (%s)\n", buf);

    /* Asynchronous write and read, with the completion event queued. */
    st = viEnableEvent(vi, VI_EVENT_IO_COMPLETION, VI_QUEUE, VI_NULL);
    check("viEnableEvent I/O completion", st == VI_SUCCESS, st);
    st = viWriteAsync(vi, (ViConstBuf)"*IDN?\n", 6, &job);
    check("viWriteAsync answers VI_SUCCESS_SYNC", st == VI_SUCCESS_SYNC, st);
    st = viWaitOnEvent(vi, VI_EVENT_IO_COMPLETION, 1000, &ty, &ev);
    check("its completion event arrives", st == VI_SUCCESS && ty == VI_EVENT_IO_COMPLETION, st);
    if (st == VI_SUCCESS) {
        ViStatus done = -1;
        ViJobId id = 0;
        ViUInt32 count = 0;
        viGetAttribute(ev, VI_ATTR_STATUS, &done);
        viGetAttribute(ev, VI_ATTR_JOB_ID, &id);
        viGetAttribute(ev, VI_ATTR_RET_COUNT_32, &count);
        viGetAttribute(ev, VI_ATTR_OPER_NAME, name);
        check("...with the job's status, id, count and name",
              done == VI_SUCCESS && id == job && count == 6 && strcmp(name, "viWriteAsync") == 0, done);
        viClose(ev);
    }
    memset(buf, 0, sizeof buf);
    st = viReadAsync(vi, (ViPBuf)buf, sizeof buf - 1, &job);
    check("viReadAsync answers VI_SUCCESS_SYNC", st == VI_SUCCESS_SYNC, st);
    st = viWaitOnEvent(vi, VI_EVENT_IO_COMPLETION, 1000, &ty, &ev);
    if (st == VI_SUCCESS) {
        ViStatus done = -1;
        ViUInt32 count = 0;
        ViBuf where = NULL;
        viGetAttribute(ev, VI_ATTR_STATUS, &done);
        viGetAttribute(ev, VI_ATTR_RET_COUNT_32, &count);
        viGetAttribute(ev, VI_ATTR_BUFFER, &where);
        check("the read's event: the answer, in the caller's buffer",
              done >= VI_SUCCESS && count > 0 && where == (ViBuf)buf && strchr(buf, ',') != NULL, done);
        viClose(ev);
    } else {
        check("the read's completion event arrives", 0, st);
    }
    st = viTerminate(vi, VI_NULL, job);
    check("viTerminate: nothing is pending (it completed)", st == VI_ERROR_INV_JOB_ID, st);
    st = viWaitOnEvent(vi, VI_EVENT_IO_COMPLETION, 0, &ty, VI_NULL);
    check("no event left over", st == VI_ERROR_TMO, st);
    viDisableEvent(vi, VI_EVENT_IO_COMPLETION, VI_ALL_MECH);

    /* A file to write from and one to read into. */
    {
        FILE *f = fopen("c_api_out.txt", "wb");
        fputs("*OPC?\n", f);
        fclose(f);
    }
    st = viWriteFromFile(vi, "c_api_out.txt", 1000, &n);
    check("viWriteFromFile sends the file", st == VI_SUCCESS && n == 6, st);
    st = viReadToFile(vi, "c_api_in.txt", 100, &n);
    check("viReadToFile takes the answer", st >= VI_SUCCESS && n >= 1, st);
    {
        FILE *f = fopen("c_api_in.txt", "rb");
        size_t k = f ? fread(buf, 1, sizeof buf - 1, f) : 0;
        if (f)
            fclose(f);
        buf[k] = 0;
        check("...and the file holds it (\"1\")", k == n && buf[0] == '1', st);
    }
    st = viReadToFile(vi, "", 10, &n);
    check("viReadToFile to a file that cannot be opened", st == VI_ERROR_FILE_ACCESS, st);

    /* The buffered forms: through the formatted I/O buffers. viBufWrite only fills the write buffer
     * (an LF in its data is data, not "\n" with END), so it is flushed to go out. viBufRead reads
     * the read buffer, where the first viQueryf's "%255[^\n]" left the answer's LF: dropped first. */
    viFlush(vi, VI_READ_BUF_DISCARD);
    st = viBufWrite(vi, (ViConstBuf)"*IDN?\n", 6, &n);
    check("viBufWrite", st == VI_SUCCESS && n == 6, st);
    st = viFlush(vi, VI_WRITE_BUF);
    check("viFlush(VI_WRITE_BUF) sends it", st == VI_SUCCESS, st);
    st = viBufRead(vi, (ViPBuf)buf, sizeof buf - 1, &n);
    buf[n] = 0;
    check("viBufRead", st >= VI_SUCCESS && strchr(buf, ',') != NULL, st);
    printf("  (%lu bytes: %s)\n", (unsigned long)n, buf);

    /* What this runtime does not do says so. */
    {
        ViUInt8 b = 0;
        st = viIn8(vi, VI_A16_SPACE, 0, &b);
        check("viIn8 is not supported", st == VI_ERROR_NSUP_OPER, st);
        st = viGpibControlATN(vi, VI_GPIB_ATN_ASSERT);
        check("viGpibControlATN is not supported", st == VI_ERROR_NSUP_OPER, st);
        st = viIn8(0x7FFFFFF0, VI_A16_SPACE, 0, &b);
        check("...and a bad session is still a bad session", st == VI_ERROR_INV_OBJECT, st);
    }

 /* VISA's own conversions. */
    {
        char idn[256];
        st = viQueryf(vi, "*IDN?\n", "%t", idn);
        check("%t reads the whole answer, to its END", st == VI_SUCCESS && strchr(idn, ',') && idn[strlen(idn) - 1] == '\n', st);
        if (strstr(idn, "33220A") || strstr(idn, "33210A")) {
            static ViInt16 pts[1000];
            for (int i = 0; i < 1000; i++)
                pts[i] = (ViInt16)((i % 100) * 163 - 8150);    /* a ramp inside the DAC's -8191..8191 */
            st = viPrintf(vi, "DATA:DAC VOLATILE, %*hb\n", 1000L, pts);
            check("%*hb: a 1000-point waveform as a definite block", st == VI_SUCCESS, st);
            int points = 0;
            st = viQueryf(vi, "DATA:ATTR:POIN? VOLATILE\n", "%d", &points);
            check("...which the generator took, all 1000 points", st == VI_SUCCESS && points == 1000, st);
            printf("  (%d points)\n", points);
        }
    }

    st = viQueryf(vi, "SYST:ERR?\n", "%255[^\n]", buf);
    check("error queue empty", st == VI_SUCCESS && (buf[0] == '0' || strncmp(buf, "+0", 2) == 0), st);
    printf("  (%s)\n", buf);
    viClose(vi);

    if (argc > 2) {
        ViSession usb;
        ViUInt16 got = 0;
        ViByte status[2] = {0xFF, 0xFF};
        st = viOpen(rm, argv[2], VI_NULL, 2000, &usb);
        check("viOpen USBTMC", st == VI_SUCCESS, st);
        if (st == VI_SUCCESS) {
            /* GET_STATUS (a standard request) to the device: two bytes, self-powered and remote wakeup. */
            st = viUsbControlIn(usb, 0x80, 0, 0, 0, 2, status, &got);
            check("viUsbControlIn GET_STATUS", st == VI_SUCCESS && got == 2 && status[1] == 0, st);
            printf("  (status %02X %02X)\n", status[0], status[1]);
            st = viUsbControlIn(usb, 0x00, 0, 0, 0, 2, status, &got);
            check("viUsbControlIn with an OUT request type is refused", st == VI_ERROR_INV_PARAMETER, st);
            viSetAttribute(usb, VI_ATTR_TMO_VALUE, 5000);
            viWrite(usb, (ViConstBuf)"*CLS\n", 5, &n);
            st = viQueryf(usb, "*IDN?\n", "%t", buf);
            if (st == VI_SUCCESS && strstr(buf, "DSO-X")) {
                ViReal64 pre[10] = {0};
                static ViByte wave[4096];
                ViInt32 npre = 10;
                long nw = sizeof wave;
                st = viPrintf(usb, ":WAV:SOUR CHAN1;:WAV:FORM BYTE;:WAV:POIN:MODE NORM;:WAV:POIN 1000\n");
                st = viQueryf(usb, ":WAV:PRE?\n", "%,#lf", &npre, pre);
                check("%,#lf: the waveform preamble, ten numbers", st == VI_SUCCESS && npre == 10 && pre[2] > 0, st);
                printf("  (preamble: format %.0f, %.0f points, x increment %g s)\n", pre[0], pre[2], pre[4]);
                st = viQueryf(usb, ":WAV:DATA?\n", "%#b", &nw, wave);
                check("%#b: the waveform as a definite block, as many bytes as the preamble says",
                      st == VI_SUCCESS && nw == (long)pre[2], st);
                printf("  (%ld bytes)\n", nw);
                st = viQueryf(usb, ":SYST:ERR?\n", "%t", buf);
                check("the oscilloscope's error queue empty", st == VI_SUCCESS && (buf[0] == '0' || strncmp(buf, "+0", 2) == 0), st);
                printf("  (%s)\n", buf);
            }
            viClose(usb);
        }
    }
    viClose(rm);
    printf("\n%s: %d failure(s)\n", fails ? "FAIL" : "PASS", fails);
    return fails != 0;
}
