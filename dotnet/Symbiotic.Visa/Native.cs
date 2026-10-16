using System;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;

namespace Symbiotic.Visa
{
    /// <summary>
    /// This runtime's C API, loaded by full path and called through function pointers. Not
    /// [DllImport("visa64")]: a process that loaded the IVI router or another vendor's visa64.dll
    /// first would have its calls bound to that one.
    /// </summary>
    internal static unsafe partial class Native
    {
        public const int VI_SUCCESS = 0;
        public const int VI_ERROR = unchecked((int)0x80000000);
        public const int VI_ERROR_INV_OBJECT = VI_ERROR + 0x3FFF000E;
        public const int VI_ERROR_RSRC_NFOUND = VI_ERROR + 0x3FFF0011;
        public const int VI_ERROR_TMO = VI_ERROR + 0x3FFF0015;
        public const int VI_ERROR_NSUP_ATTR = VI_ERROR + 0x3FFF001D;
        public const int VI_ERROR_ABORT = VI_ERROR + 0x3FFF0030;
        public const int VI_ERROR_NSUP_OPER = VI_ERROR + 0x3FFF0067;
        public const int VI_SUCCESS_EVENT_DIS = 0x3FFF0003;
        public const int VI_SUCCESS_TERM_CHAR = 0x3FFF0005;
        public const int VI_SUCCESS_MAX_CNT = 0x3FFF0006;
        public const int VI_SUCCESS_DEV_NPRESENT = 0x3FFF007D;
        public const int VI_SUCCESS_QUEUE_NEMPTY = 0x3FFF0080;
        public const int VI_WARN_QUEUE_OVERFLOW = 0x3FFF000C;
        public const int VI_WARN_CONFIG_NLOADED = 0x3FFF0077;
        public const int VI_SUCCESS_NESTED_SHARED = 0x3FFF0099;
        public const int VI_SUCCESS_NESTED_EXCLUSIVE = 0x3FFF009A;

        public const uint VI_ATTR_RSRC_CLASS = 0xBFFF0001;
        public const uint VI_ATTR_RSRC_NAME = 0xBFFF0002;
        public const uint VI_ATTR_RSRC_IMPL_VERSION = 0x3FFF0003;
        public const uint VI_ATTR_RSRC_LOCK_STATE = 0x3FFF0004;
        public const uint VI_ATTR_MAX_QUEUE_LENGTH = 0x3FFF0005;
        public const uint VI_ATTR_SEND_END_EN = 0x3FFF0016;
        public const uint VI_ATTR_TERMCHAR = 0x3FFF0018;
        public const uint VI_ATTR_TMO_VALUE = 0x3FFF001A;
        public const uint VI_ATTR_GPIB_READDR_EN = 0x3FFF001B;
        public const uint VI_ATTR_IO_PROT = 0x3FFF001C;
        public const uint VI_ATTR_DMA_ALLOW_EN = 0x3FFF001E;
        public const uint VI_ATTR_ASRL_BAUD = 0x3FFF0021;
        public const uint VI_ATTR_ASRL_DATA_BITS = 0x3FFF0022;
        public const uint VI_ATTR_ASRL_PARITY = 0x3FFF0023;
        public const uint VI_ATTR_ASRL_STOP_BITS = 0x3FFF0024;
        public const uint VI_ATTR_ASRL_FLOW_CNTRL = 0x3FFF0025;
        public const uint VI_ATTR_TERMCHAR_EN = 0x3FFF0038;
        public const uint VI_ATTR_GPIB_ATN_STATE = 0x3FFF0057;
        public const uint VI_ATTR_GPIB_ADDR_STATE = 0x3FFF005C;
        public const uint VI_ATTR_GPIB_CIC_STATE = 0x3FFF005E;
        public const uint VI_ATTR_GPIB_NDAC_STATE = 0x3FFF0062;
        public const uint VI_ATTR_GPIB_SRQ_STATE = 0x3FFF0067;
        public const uint VI_ATTR_GPIB_SYS_CNTRL_STATE = 0x3FFF0068;
        public const uint VI_ATTR_GPIB_HS488_CBL_LEN = 0x3FFF0069;
        public const uint VI_ATTR_ASRL_AVAIL_NUM = 0x3FFF00AC;
        public const uint VI_ATTR_ASRL_CTS_STATE = 0x3FFF00AE;
        public const uint VI_ATTR_ASRL_DCD_STATE = 0x3FFF00AF;
        public const uint VI_ATTR_ASRL_DSR_STATE = 0x3FFF00B1;
        public const uint VI_ATTR_ASRL_DTR_STATE = 0x3FFF00B2;
        public const uint VI_ATTR_ASRL_END_IN = 0x3FFF00B3;
        public const uint VI_ATTR_ASRL_END_OUT = 0x3FFF00B4;
        public const uint VI_ATTR_ASRL_REPLACE_CHAR = 0x3FFF00BE;
        public const uint VI_ATTR_ASRL_RI_STATE = 0x3FFF00BF;
        public const uint VI_ATTR_ASRL_RTS_STATE = 0x3FFF00C0;
        public const uint VI_ATTR_ASRL_XON_CHAR = 0x3FFF00C1;
        public const uint VI_ATTR_ASRL_XOFF_CHAR = 0x3FFF00C2;
        public const uint VI_ATTR_MANF_ID = 0x3FFF00D9;
        public const uint VI_ATTR_MODEL_CODE = 0x3FFF00DF;
        public const uint VI_ATTR_MANF_NAME = 0xBFFF0072;
        public const uint VI_ATTR_MODEL_NAME = 0xBFFF0077;
        public const uint VI_ATTR_INTF_INST_NAME = 0xBFFF00E9;
        public const uint VI_ATTR_RSRC_SPEC_VERSION = 0x3FFF0170;
        public const uint VI_ATTR_INTF_TYPE = 0x3FFF0171;
        public const uint VI_ATTR_GPIB_PRIMARY_ADDR = 0x3FFF0172;
        public const uint VI_ATTR_GPIB_SECONDARY_ADDR = 0x3FFF0173;
        public const uint VI_ATTR_RSRC_MANF_NAME = 0xBFFF0174;
        public const uint VI_ATTR_INTF_NUM = 0x3FFF0176;
        public const uint VI_ATTR_GPIB_REN_STATE = 0x3FFF0181;
        public const uint VI_ATTR_GPIB_UNADDR_EN = 0x3FFF0184;
        public const uint VI_ATTR_DEV_STATUS_BYTE = 0x3FFF0189;
        public const uint VI_ATTR_TCPIP_ADDR = 0xBFFF0195;
        public const uint VI_ATTR_TCPIP_HOSTNAME = 0xBFFF0196;
        public const uint VI_ATTR_TCPIP_PORT = 0x3FFF0197;
        public const uint VI_ATTR_TCPIP_DEVICE_NAME = 0xBFFF0199;
        public const uint VI_ATTR_TCPIP_NODELAY = 0x3FFF019A;
        public const uint VI_ATTR_TCPIP_KEEPALIVE = 0x3FFF019B;
        public const uint VI_ATTR_4882_COMPLIANT = 0x3FFF019F;
        public const uint VI_ATTR_USB_SERIAL_NUM = 0xBFFF01A0;
        public const uint VI_ATTR_USB_INTFC_NUM = 0x3FFF01A1;
        public const uint VI_ATTR_USB_PROTOCOL = 0x3FFF01A7;
        public const uint VI_ATTR_USB_MAX_INTR_SIZE = 0x3FFF01AF;
        public const uint VI_ATTR_TCPIP_HISLIP_OVERLAP_EN = 0x3FFF0300;
        public const uint VI_ATTR_TCPIP_HISLIP_VERSION = 0x3FFF0301;
        public const uint VI_ATTR_TCPIP_HISLIP_MAX_MESSAGE_KB = 0x3FFF0302;
        public const uint VI_ATTR_TCPIP_IS_HISLIP = 0x3FFF0303;
        public const uint VI_ATTR_JOB_ID = 0x3FFF4006;
        public const uint VI_ATTR_EVENT_TYPE = 0x3FFF4010;
        public const uint VI_ATTR_STATUS = 0x3FFF4025;
        public const uint VI_ATTR_BUFFER = 0x3FFF4027;
        public const uint VI_ATTR_RET_COUNT_32 = 0x3FFF4026;
        public const uint VI_ATTR_RET_COUNT_64 = 0x3FFF4028;
        public const uint VI_ATTR_OPER_NAME = 0xBFFF4042;

        public const uint VI_EVENT_IO_COMPLETION = 0x3FFF2009;
        public const uint VI_EVENT_TRIG = 0xBFFF200A;
        public const uint VI_EVENT_SERVICE_REQ = 0x3FFF200B;
        public const uint VI_EVENT_CLEAR = 0x3FFF200D;
        public const uint VI_EVENT_EXCEPTION = 0xBFFF200E;
        public const uint VI_EVENT_GPIB_CIC = 0x3FFF2012;
        public const uint VI_EVENT_GPIB_TALK = 0x3FFF2013;
        public const uint VI_EVENT_GPIB_LISTEN = 0x3FFF2014;
        public const uint VI_EVENT_USB_INTR = 0x3FFF2037;
        public const uint VI_ALL_ENABLED_EVENTS = 0x3FFF7FFF;

        public const ushort VI_QUEUE = 1;
        public const ushort VI_HNDLR = 2;
        public const ushort VI_ALL_MECH = 0xFFFF;
        public const uint VI_NO_LOCK = 0;
        public const uint VI_EXCLUSIVE_LOCK = 1;
        public const uint VI_SHARED_LOCK = 2;
        public const uint VI_LOAD_CONFIG = 4;
        public const uint VI_TMO_INFINITE = 0xFFFFFFFF;
        public const ushort VI_TRIG_PROT_DEFAULT = 0;

        public const ushort VI_READ_BUF = 1;
        public const ushort VI_WRITE_BUF = 2;
        public const ushort VI_READ_BUF_DISCARD = 4;
        public const ushort VI_WRITE_BUF_DISCARD = 8;
        public const ushort VI_IO_IN_BUF = 16;
        public const ushort VI_IO_OUT_BUF = 32;
        public const ushort VI_IO_IN_BUF_DISCARD = 64;
        public const ushort VI_IO_OUT_BUF_DISCARD = 128;

        public const ushort VI_INTF_GPIB = 1;
        public const ushort VI_INTF_ASRL = 4;
        public const ushort VI_INTF_TCPIP = 6;
        public const ushort VI_INTF_USB = 7;

        /// <summary>Long enough for any string attribute or resource name (VI_FIND_BUFLEN is 256).</summary>
        public const int StringLength = 256;

        static readonly object _lock = new object();
        static IntPtr _module;
        static string _path;

        public static delegate* unmanaged[Stdcall]<uint*, int> viOpenDefaultRM;
        public static delegate* unmanaged[Stdcall]<uint, byte*, uint*, uint*, byte*, int> viFindRsrc;
        public static delegate* unmanaged[Stdcall]<uint, byte*, int> viFindNext;
        public static delegate* unmanaged[Stdcall]<uint, byte*, ushort*, ushort*, byte*, byte*, byte*, int> viParseRsrcEx;
        public static delegate* unmanaged[Stdcall]<uint, byte*, uint, uint, uint*, int> viOpen;
        public static delegate* unmanaged[Stdcall]<uint, int> viClose;
        public static delegate* unmanaged[Stdcall]<uint, uint, nuint, int> viSetAttribute;
        public static delegate* unmanaged[Stdcall]<uint, uint, void*, int> viGetAttribute;
        public static delegate* unmanaged[Stdcall]<uint, int, byte*, int> viStatusDesc;
        public static delegate* unmanaged[Stdcall]<uint, ushort, uint, int> viTerminate;
        public static delegate* unmanaged[Stdcall]<uint, uint, uint, byte*, byte*, int> viLock;
        public static delegate* unmanaged[Stdcall]<uint, int> viUnlock;
        public static delegate* unmanaged[Stdcall]<uint, uint, ushort, uint, int> viEnableEvent;
        public static delegate* unmanaged[Stdcall]<uint, uint, ushort, int> viDisableEvent;
        public static delegate* unmanaged[Stdcall]<uint, uint, ushort, int> viDiscardEvents;
        public static delegate* unmanaged[Stdcall]<uint, uint, uint, uint*, uint*, int> viWaitOnEvent;
        public static delegate* unmanaged[Stdcall]<uint, uint, IntPtr, IntPtr, int> viInstallHandler;
        public static delegate* unmanaged[Stdcall]<uint, uint, IntPtr, IntPtr, int> viUninstallHandler;
        public static delegate* unmanaged[Stdcall]<uint, byte*, uint, uint*, int> viRead;
        public static delegate* unmanaged[Stdcall]<uint, byte*, uint, uint*, int> viReadAsync;
        public static delegate* unmanaged[Stdcall]<uint, byte*, uint, uint*, int> viWrite;
        public static delegate* unmanaged[Stdcall]<uint, byte*, uint, uint*, int> viWriteAsync;
        public static delegate* unmanaged[Stdcall]<uint, ushort, int> viAssertTrigger;
        public static delegate* unmanaged[Stdcall]<uint, ushort*, int> viReadSTB;
        public static delegate* unmanaged[Stdcall]<uint, int> viClear;
        public static delegate* unmanaged[Stdcall]<uint, ushort, uint, int> viSetBuf;
        public static delegate* unmanaged[Stdcall]<uint, ushort, int> viFlush;
        public static delegate* unmanaged[Stdcall]<uint, ushort, int> viGpibControlREN;
        public static delegate* unmanaged[Stdcall]<uint, ushort, int> viGpibControlATN;
        public static delegate* unmanaged[Stdcall]<uint, int> viGpibSendIFC;
        public static delegate* unmanaged[Stdcall]<uint, byte*, uint, uint*, int> viGpibCommand;
        public static delegate* unmanaged[Stdcall]<uint, ushort, ushort, int> viGpibPassControl;
        public static delegate* unmanaged[Stdcall]<uint, short, short, ushort, ushort, ushort, byte*, int> viUsbControlOut;
        public static delegate* unmanaged[Stdcall]<uint, short, short, ushort, ushort, ushort, byte*, ushort*, int> viUsbControlIn;

        /// <summary>Where the C library was loaded from (for messages and the tests).</summary>
        public static string LibraryPath
        {
            get { Load(); return _path; }
        }

        [DllImport("kernel32", CharSet = CharSet.Unicode, SetLastError = true)]
        static extern IntPtr LoadLibraryW(string path);

        [DllImport("kernel32", CharSet = CharSet.Ansi, ExactSpelling = true)]
        static extern IntPtr GetProcAddress(IntPtr module, string name);

        static string LibraryName => IntPtr.Size == 8 ? "visa64.dll" : "visa32.dll";

        /// <summary>
        /// The C library this assembly drives: SYMBIOTIC_VISA_DLL if set (development); next to this
        /// assembly if the daemon is there too (a build tree); else the all-users install
        /// (HKLM\SOFTWARE\Viper488\Symbiotic\InstallDir, the 64-bit view whatever this process is).
        /// Never a visa64.dll found by name, nor one in the user's profile: a folder the user can
        /// write, which an elevated process would then run their code from.
        /// </summary>
        static string Locate()
        {
            string env = Environment.GetEnvironmentVariable("SYMBIOTIC_VISA_DLL");
            if (!string.IsNullOrEmpty(env))
                return env;
            string here = null;
            try { here = Path.GetDirectoryName(typeof(Native).Assembly.Location); } catch (Exception) { }
            if (!string.IsNullOrEmpty(here) && File.Exists(Path.Combine(here, LibraryName)) && File.Exists(Path.Combine(here, "symbioticd.exe")))
                return Path.Combine(here, LibraryName);
#if NET
            if (OperatingSystem.IsWindows())
#endif
            {
                string dir = MachineInstallDir();
                if (dir != null && File.Exists(Path.Combine(dir, LibraryName)))
                    return Path.Combine(dir, LibraryName);
            }
            throw LoadFailure($"Viper488 Symbiotic VISA is not installed (no {LibraryName} under HKLM\\SOFTWARE\\Viper488\\Symbiotic\\InstallDir); install it, or set SYMBIOTIC_VISA_DLL");
        }

#if NET
        [System.Runtime.Versioning.SupportedOSPlatform("windows")]
#endif
        static string MachineInstallDir()
        {
            try
            {
                using (var hklm = Microsoft.Win32.RegistryKey.OpenBaseKey(Microsoft.Win32.RegistryHive.LocalMachine, Microsoft.Win32.RegistryView.Registry64))
                using (var k = hklm.OpenSubKey(@"SOFTWARE\Viper488\Symbiotic"))
                    return k?.GetValue("InstallDir") as string;
            }
            catch (Exception)
            {
                return null;
            }
        }

        /// <summary>Load the library once; a failure is a VisaException that says where it looked.</summary>
        public static void Load()
        {
            if (_module != IntPtr.Zero)
                return;
            lock (_lock)
            {
                if (_module != IntPtr.Zero)
                    return;
                string path = Locate();
                IntPtr m = LoadLibraryW(path);
                if (m == IntPtr.Zero)
                    throw LoadFailure($"cannot load {path} (Win32 error {Marshal.GetLastWin32Error()}): is Viper488 Symbiotic VISA installed?");
                viOpenDefaultRM = (delegate* unmanaged[Stdcall]<uint*, int>)Get(m, path, "viOpenDefaultRM");
                viFindRsrc = (delegate* unmanaged[Stdcall]<uint, byte*, uint*, uint*, byte*, int>)Get(m, path, "viFindRsrc");
                viFindNext = (delegate* unmanaged[Stdcall]<uint, byte*, int>)Get(m, path, "viFindNext");
                viParseRsrcEx = (delegate* unmanaged[Stdcall]<uint, byte*, ushort*, ushort*, byte*, byte*, byte*, int>)Get(m, path, "viParseRsrcEx");
                viOpen = (delegate* unmanaged[Stdcall]<uint, byte*, uint, uint, uint*, int>)Get(m, path, "viOpen");
                viClose = (delegate* unmanaged[Stdcall]<uint, int>)Get(m, path, "viClose");
                viSetAttribute = (delegate* unmanaged[Stdcall]<uint, uint, nuint, int>)Get(m, path, "viSetAttribute");
                viGetAttribute = (delegate* unmanaged[Stdcall]<uint, uint, void*, int>)Get(m, path, "viGetAttribute");
                viStatusDesc = (delegate* unmanaged[Stdcall]<uint, int, byte*, int>)Get(m, path, "viStatusDesc");
                viTerminate = (delegate* unmanaged[Stdcall]<uint, ushort, uint, int>)Get(m, path, "viTerminate");
                viLock = (delegate* unmanaged[Stdcall]<uint, uint, uint, byte*, byte*, int>)Get(m, path, "viLock");
                viUnlock = (delegate* unmanaged[Stdcall]<uint, int>)Get(m, path, "viUnlock");
                viEnableEvent = (delegate* unmanaged[Stdcall]<uint, uint, ushort, uint, int>)Get(m, path, "viEnableEvent");
                viDisableEvent = (delegate* unmanaged[Stdcall]<uint, uint, ushort, int>)Get(m, path, "viDisableEvent");
                viDiscardEvents = (delegate* unmanaged[Stdcall]<uint, uint, ushort, int>)Get(m, path, "viDiscardEvents");
                viWaitOnEvent = (delegate* unmanaged[Stdcall]<uint, uint, uint, uint*, uint*, int>)Get(m, path, "viWaitOnEvent");
                viInstallHandler = (delegate* unmanaged[Stdcall]<uint, uint, IntPtr, IntPtr, int>)Get(m, path, "viInstallHandler");
                viUninstallHandler = (delegate* unmanaged[Stdcall]<uint, uint, IntPtr, IntPtr, int>)Get(m, path, "viUninstallHandler");
                viRead = (delegate* unmanaged[Stdcall]<uint, byte*, uint, uint*, int>)Get(m, path, "viRead");
                viReadAsync = (delegate* unmanaged[Stdcall]<uint, byte*, uint, uint*, int>)Get(m, path, "viReadAsync");
                viWrite = (delegate* unmanaged[Stdcall]<uint, byte*, uint, uint*, int>)Get(m, path, "viWrite");
                viWriteAsync = (delegate* unmanaged[Stdcall]<uint, byte*, uint, uint*, int>)Get(m, path, "viWriteAsync");
                viAssertTrigger = (delegate* unmanaged[Stdcall]<uint, ushort, int>)Get(m, path, "viAssertTrigger");
                viReadSTB = (delegate* unmanaged[Stdcall]<uint, ushort*, int>)Get(m, path, "viReadSTB");
                viClear = (delegate* unmanaged[Stdcall]<uint, int>)Get(m, path, "viClear");
                viSetBuf = (delegate* unmanaged[Stdcall]<uint, ushort, uint, int>)Get(m, path, "viSetBuf");
                viFlush = (delegate* unmanaged[Stdcall]<uint, ushort, int>)Get(m, path, "viFlush");
                viGpibControlREN = (delegate* unmanaged[Stdcall]<uint, ushort, int>)Get(m, path, "viGpibControlREN");
                viGpibControlATN = (delegate* unmanaged[Stdcall]<uint, ushort, int>)Get(m, path, "viGpibControlATN");
                viGpibSendIFC = (delegate* unmanaged[Stdcall]<uint, int>)Get(m, path, "viGpibSendIFC");
                viGpibCommand = (delegate* unmanaged[Stdcall]<uint, byte*, uint, uint*, int>)Get(m, path, "viGpibCommand");
                viGpibPassControl = (delegate* unmanaged[Stdcall]<uint, ushort, ushort, int>)Get(m, path, "viGpibPassControl");
                viUsbControlOut = (delegate* unmanaged[Stdcall]<uint, short, short, ushort, ushort, ushort, byte*, int>)Get(m, path, "viUsbControlOut");
                viUsbControlIn = (delegate* unmanaged[Stdcall]<uint, short, short, ushort, ushort, ushort, byte*, ushort*, int>)Get(m, path, "viUsbControlIn");
                _path = path;
                _module = m;
            }
        }

        static IntPtr Get(IntPtr module, string path, string name)
        {
            IntPtr f = GetProcAddress(module, name);
            if (f == IntPtr.Zero)
                throw LoadFailure($"{path} does not export {name}");
            return f;
        }

        // ------------------------------------------------------------------------------------
        // Strings: the C API's are 8-bit, NUL-terminated.

        public static byte[] Z(string s)
        {
            byte[] b = new byte[Encoding.ASCII.GetByteCount(s ?? "") + 1];
            Encoding.ASCII.GetBytes(s ?? "", 0, (s ?? "").Length, b, 0);
            return b;
        }

        public static string FromZ(byte* p, int max = StringLength)
        {
            int n = 0;
            while (n < max && p[n] != 0)
                n++;
            return Encoding.ASCII.GetString(p, n);
        }

        // ------------------------------------------------------------------------------------
        // Status codes to exceptions: negative is an error, positive a completion code.

        public static string Describe(uint vi, int status)
        {
            byte* buf = stackalloc byte[StringLength];
            if (viStatusDesc(vi, status, buf) >= 0)
            {
                string s = FromZ(buf);
                if (s.Length > 0)
                    return s;
            }
            return $"VISA status 0x{status:X8}";
        }

        /// <summary>Throw on an error status; return the status (a completion code or warning) otherwise.</summary>
        public static int Check(uint vi, int status)
        {
            if (status < 0)
                throw Error(vi, status);
            return status;
        }

        // Each assembly that compiles this file says what its failures are (NativeErrors.cs): VISA.NET's
        // exceptions in Symbiotic.Visa, COM errors carrying the status as their HRESULT in Symbiotic.VisaCom.
        static partial void MakeLoadFailure(string message, ref Exception e);
        static partial void MakeError(uint vi, int status, ref Exception e);

        static Exception LoadFailure(string message)
        {
            Exception e = null;
            MakeLoadFailure(message, ref e);
            return e ?? new InvalidOperationException(message);
        }

        public static Exception Error(uint vi, int status)
        {
            Exception e = null;
            MakeError(vi, status, ref e);
            return e ?? new InvalidOperationException(Describe(vi, status));
        }
    }
}
