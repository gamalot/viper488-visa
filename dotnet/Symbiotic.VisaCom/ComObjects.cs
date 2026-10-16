using System;
using System.Runtime.InteropServices;
using Symbiotic.Visa;

namespace Symbiotic.Visa
{
    /// <summary>VISA COM's side of Native's failures: a COM error whose HRESULT is the VISA status (VPP-4.3.4).</summary>
    internal static partial class Native
    {
        static partial void MakeLoadFailure(string message, ref Exception e) =>
            e = new Symbiotic.VisaCom.VisaComException(VI_ERROR_SYSTEM_ERROR, message);

        static partial void MakeError(uint vi, int status, ref Exception e) =>
            e = new Symbiotic.VisaCom.VisaComException(status, Describe(vi, status));

        public const int VI_ERROR_SYSTEM_ERROR = VI_ERROR + 0x3FFF0000;
    }
}

namespace Symbiotic.VisaCom
{
    /// <summary>A VISA status as a COM error: the HRESULT is the status. What a .NET caller of these objects
    /// catches (as a COMException, ErrorCode the status).</summary>
    public class VisaComException : COMException
    {
        public VisaComException(int status, string message) : base(message, status) { }
    }

    internal static class Status
    {
        public const int E_INVALIDARG = unchecked((int)0x80070057);
        public const int VI_ERROR_INV_SETUP = Native.VI_ERROR + 0x3FFF003A;
        public const int VI_ERROR_ATTR_READONLY = Native.VI_ERROR + 0x3FFF001F;
        public const int VI_ERROR_NSUP_ATTR_STATE = Native.VI_ERROR + 0x3FFF001E;
        public const int VI_ERROR_INV_SESSION = Native.VI_ERROR + 0x3FFF000E;
        public const int VI_ERROR_INV_PARAMETER = Native.VI_ERROR + 0x3FFF0078;
        public const int VI_ERROR_INV_EVENT = Native.VI_ERROR + 0x3FFF0026;
        public const int VI_ERROR_HNDLR_NINSTALLED = Native.VI_ERROR + 0x3FFF0028;

        public static VisaComException Error(int status, string message) => new VisaComException(status, message);
    }

    /// <summary>The COM plumbing every object here shares: completion codes out as success HRESULTs, errors as
    /// failure HRESULTs with a description for IErrorInfo (the [PreserveSig] twins call Begin, End, Fail).</summary>
    [ComVisible(false)]
    public abstract class ComObject : ICustomQueryInterface
    {
        /// <summary>Every interface asked for, when SYMBIOTIC_VISACOM_TRACE is set: what a caller wants that is not
        /// here shows up as the last IID before its E_NOINTERFACE.</summary>
        CustomQueryInterfaceResult ICustomQueryInterface.GetInterface(ref Guid iid, out IntPtr ppv)
        {
            ppv = IntPtr.Zero;
            Trace.Log($"{GetType().Name} QueryInterface {iid:B}");
            return CustomQueryInterfaceResult.NotHandled;
        }

        /// <summary>The completion code of the call under way on this thread (a VISA success code, 0 by default).</summary>
        [ThreadStatic] static int t_completion;

        /// <summary>What the last operation returned, for IVisaSession.LastStatus.</summary>
        protected int _lastStatus;

        /// <summary>A C status: an error is thrown (and remembered), a completion code kept for the caller's HRESULT.</summary>
        internal int Check(uint vi, int status)
        {
            _lastStatus = status;
            if (status < 0)
                throw Native.Error(vi, status);
            t_completion = status;
            return status;
        }

        internal void Begin() => t_completion = 0;

        internal int End() => t_completion;

        internal int Fail(Exception e)
        {
            int hr = e.HResult < 0 ? e.HResult : unchecked((int)0x80004005); // E_FAIL
            Trace.Log($"{GetType().Name} fails 0x{hr:X8}: {e}");
            _lastStatus = hr;
            ErrorInfo.Set(GetType().FullName, e.Message);
            return hr;
        }
    }

    /// <summary>IErrorInfo for the thread, so a COM caller (VBA, C#, a driver) sees the message and not only the number.</summary>
    internal static class ErrorInfo
    {
        [ComImport, Guid("22F03340-547D-101B-8E65-08002B2BD119"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
        interface ICreateErrorInfo
        {
            void SetGUID(ref Guid rguid);
            void SetSource([MarshalAs(UnmanagedType.LPWStr)] string szSource);
            void SetDescription([MarshalAs(UnmanagedType.LPWStr)] string szDescription);
            void SetHelpFile([MarshalAs(UnmanagedType.LPWStr)] string szHelpFile);
            void SetHelpContext(uint dwHelpContext);
        }

        [DllImport("oleaut32.dll")]
        static extern int CreateErrorInfo(out ICreateErrorInfo info);

        [DllImport("oleaut32.dll")]
        static extern int SetErrorInfo(uint reserved, IntPtr info);

        public static void Set(string source, string description)
        {
            try
            {
                if (CreateErrorInfo(out ICreateErrorInfo c) < 0 || c == null)
                    return;
                c.SetSource(source);
                c.SetDescription(description);
                IntPtr unk = Marshal.GetIUnknownForObject(c);
                try
                {
                    Guid iid = new Guid("1CF2B120-547D-101B-8E65-08002B2BD119"); // IErrorInfo
                    if (Marshal.QueryInterface(unk, ref iid, out IntPtr info) >= 0)
                    {
                        SetErrorInfo(0, info);
                        Marshal.Release(info);
                    }
                }
                finally
                {
                    Marshal.Release(unk);
                }
            }
            catch (Exception)
            {
                // Only the description is lost; the HRESULT still says what happened.
            }
        }
    }

    internal static class ComObjects
    {
        /// <summary>The object's pointer for one interface, AddRef'd for the caller: through QueryInterface, so a
        /// COM caller gets the [PreserveSig] twin's vtable (see Generated.cs), not the interop type's.</summary>
        public static IntPtr Pointer(object o, Guid iid)
        {
            if (o == null)
                return IntPtr.Zero;
            IntPtr unk = Marshal.GetIUnknownForObject(o);
            try
            {
                int hr = Marshal.QueryInterface(unk, ref iid, out IntPtr p);
                if (hr < 0)
                    Marshal.ThrowExceptionForHR(hr);
                return p;
            }
            finally
            {
                Marshal.Release(unk);
            }
        }

        /// <summary>VPP-4.3's version word from an assembly version: major 12 bits, minor 12, sub-minor 8.</summary>
        public static int VersionWord(Version v) =>
            (Math.Min(v.Major, 0xFFF) << 20) | (Math.Min(v.Minor, 0xFFF) << 8) | Math.Min(Math.Max(v.Build, 0), 0xFF);
    }
}

namespace Symbiotic.VisaCom
{
    internal static class Trace
    {
        static readonly string _path = Environment.GetEnvironmentVariable("SYMBIOTIC_VISACOM_TRACE") != null
            ? System.IO.Path.Combine(System.IO.Path.GetTempPath(), "symbiotic-visacom.log") : null;

        public static void Log(string line)
        {
            if (_path == null)
                return;
            try
            {
                lock (typeof(Trace))
                    System.IO.File.AppendAllText(_path, $"{DateTime.Now:HH:mm:ss.fff} [{System.Diagnostics.Process.GetCurrentProcess().Id} {AppDomain.CurrentDomain.Id}] {line}\r\n");
            }
            catch (Exception)
            {
            }
        }
    }
}
