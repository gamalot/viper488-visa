using System;
using System.IO;
using System.Runtime.InteropServices;
using Symbiotic.Visa;

namespace Symbiotic.VisaCom
{
    /// <summary>
    /// Where the resource manager opens its sessions: an AppDomain of their own. A .NET caller in
    /// the process that got one of our objects from its own AppDomain would get the object itself,
    /// and hand it on to native code (the shared FormattedIO488, say) through the interop type's
    /// vtable, whose reads cannot return VI_SUCCESS_MAX_CNT; one from another AppDomain is a plain
    /// COM object to it, reached through the [PreserveSig] twins like any.
    /// </summary>
    internal static class Sessions
    {
        static AppDomain _domain;
        static readonly object _lock = new object();

        static AppDomain Domain
        {
            get
            {
                if (_domain == null)
                {
                    var setup = new AppDomainSetup { ApplicationBase = Path.GetDirectoryName(typeof(Sessions).Assembly.Location) };
                    _domain = AppDomain.CreateDomain("Symbiotic VISA COM sessions", null, setup);
                }
                return _domain;
            }
        }

        /// <summary>
        /// SessionFactory.Open in the private domain. Only strings and numbers cross: COM activation
        /// loads this assembly by its codebase (the LoadFrom context), and a remoting proxy's type
        /// would be looked for by name in the Load context and not found there, or found as another
        /// copy. A static method's delegate and AppDomain data need no such lookup here.
        /// </summary>
        public static int Open(string resourceName, int mode, int timeout, string options, out long unk, out string message)
        {
            lock (_lock)
            {
                AppDomain d = Domain;
                d.SetData("in.name", resourceName);
                d.SetData("in.mode", mode);
                d.SetData("in.timeout", timeout);
                d.SetData("in.options", options);
                d.DoCallBack(SessionFactory.OpenHere);
                unk = (long)d.GetData("out.unk");
                message = (string)d.GetData("out.message");
                return (int)d.GetData("out.status");
            }
        }
    }

    /// <summary>Opens sessions inside the private domain; statuses and messages cross the boundary, not exceptions.</summary>
    internal static unsafe class SessionFactory
    {
        /// <summary>Runs in the private domain (Sessions.Open's DoCallBack): its arguments and results are the domain's data.</summary>
        public static void OpenHere()
        {
            AppDomain d = AppDomain.CurrentDomain;
            int st = Open((string)d.GetData("in.name"), (int)d.GetData("in.mode"), (int)d.GetData("in.timeout"), (string)d.GetData("in.options"),
                out long unk, out string message);
            d.SetData("out.status", st);
            d.SetData("out.unk", unk);
            d.SetData("out.message", message);
        }

        /// <summary>The session's IVisaSession pointer (AddRef'd, the twin's vtable) and the open's status.</summary>
        static int Open(string resourceName, int mode, int timeout, string options, out long unk, out string message)
        {
            unk = 0;
            message = null;
            try
            {
                Native.Load();
                uint rm;
                ushort type, num;
                byte* cls = stackalloc byte[Native.StringLength];
                int st = Native.viOpenDefaultRM(&rm);
                if (st < 0)
                    throw Native.Error(0, st);
                try
                {
                    byte[] name = Native.Z(resourceName ?? "");
                    fixed (byte* n = name)
                        st = Native.viParseRsrcEx(rm, n, &type, &num, cls, null, null);
                    if (st < 0)
                        throw Native.Error(rm, st);
                }
                finally
                {
                    Native.viClose(rm);
                }
                string c = Native.FromZ(cls);
                SessionBase s;
                switch (type)
                {
                    case Native.VI_INTF_GPIB: s = c == "INTFC" ? (SessionBase)new GpibIntfc() : new GpibInstr(); break;
                    case Native.VI_INTF_TCPIP: s = c == "SOCKET" ? (SessionBase)new TcpipSocket() : new TcpipInstr(); break;
                    case Native.VI_INTF_USB: s = new UsbInstr(); break;
                    case Native.VI_INTF_ASRL: s = new AsrlInstr(); break;
                    default: throw Status.Error(Native.VI_ERROR_RSRC_NFOUND, $"{resourceName}: no VISA COM resource of interface type {type} here");
                }
                s.Init(resourceName, (Ivi.Visa.Interop.AccessMode)mode, timeout, options);
                unk = (long)ComObjects.Pointer(s, typeof(Twins.IVisaSessionTwin).GUID);
                return s.LastStatus;
            }
            catch (Exception e)
            {
                message = e.Message;
                return e.HResult < 0 ? e.HResult : unchecked((int)0x80004005);
            }
        }
    }
}
