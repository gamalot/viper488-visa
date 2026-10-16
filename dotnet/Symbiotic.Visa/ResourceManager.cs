using System;
using System.Collections.Generic;
using Ivi.Visa;

namespace Symbiotic.Visa
{
    /// <summary>
    /// The vendor-specific resource manager the IVI Global Resource Manager instantiates by its
    /// registered Location. Programs may also create it directly.
    /// </summary>
    public sealed unsafe class ResourceManager : IResourceManager
    {
        /// <summary>"vy" packed as VPP-4.3.6 17.3.2.3 has it: two lower-case letters, big-endian.
        /// Chosen here, not assigned by VPP-9 (nobody has VY in its 4.35 edition).</summary>
        public const short VendorId = 0x7679;
        public const string VendorName = "Viper488 Symbiotic";

        uint _rm;

        public ResourceManager()
        {
            Native.Load();
            uint rm;
            Native.Check(0, Native.viOpenDefaultRM(&rm));
            _rm = rm;
        }

        public string ManufacturerName => VendorName;

        public short ManufacturerId => VendorId;

        public Version ImplementationVersion => ImplVersion;

        public Version SpecificationVersion => SpecVersion;

        /// <summary>The C library this assembly drives (not a VISA property: for logs and scripts, as rm.visalib is in pyvisa).</summary>
        public static string NativeLibraryPath => Native.LibraryPath;

        /// <summary>This assembly's version (0.1.commits.0, as the DLL's resources).</summary>
        internal static Version ImplVersion => typeof(ResourceManager).Assembly.GetName().Version;

        /// <summary>The VISA.NET specification written to: VPP-4.3.6 as of 2025 (7.4, as the IVI GRM reports).</summary>
        internal static readonly Version SpecVersion = new Version(7, 4, 0, 0);

        uint Rm
        {
            get
            {
                if (_rm == 0)
                    throw new ObjectDisposedException(nameof(ResourceManager));
                return _rm;
            }
        }

        public IEnumerable<string> Find(string pattern)
        {
            if (string.IsNullOrEmpty(pattern))
                throw new ArgumentException("an empty search expression", nameof(pattern));
            var found = new List<string>();
            byte[] expr = Native.Z(pattern);
            byte* desc = stackalloc byte[Native.StringLength];
            uint list = 0, count = 0;
            int st;
            fixed (byte* e = expr)
                st = Native.viFindRsrc(Rm, e, &list, &count, desc);
            // Nothing found is an empty answer here; the GRM combines what every vendor finds.
            if (st == Native.VI_ERROR_RSRC_NFOUND)
                return found;
            Native.Check(Rm, st);
            try
            {
                found.Add(Native.FromZ(desc));
                for (uint i = 1; i < count; i++)
                {
                    Native.Check(Rm, Native.viFindNext(list, desc));
                    found.Add(Native.FromZ(desc));
                }
            }
            finally
            {
                if (list != 0)
                    Native.viClose(list);
            }
            return found;
        }

        public ParseResult Parse(string resourceName)
        {
            if (string.IsNullOrEmpty(resourceName))
                throw new ArgumentException("an empty resource name", nameof(resourceName));
            byte[] name = Native.Z(resourceName);
            ushort type, num;
            byte* cls = stackalloc byte[Native.StringLength];
            byte* expanded = stackalloc byte[Native.StringLength];
            byte* alias = stackalloc byte[Native.StringLength];
            fixed (byte* n = name)
                Native.Check(Rm, Native.viParseRsrcEx(Rm, n, &type, &num, cls, expanded, alias));
            // VI_INTF_* and HardwareInterfaceType are numbered alike (GPIB 1 ... USB 7).
            var intf = type <= 7 ? (HardwareInterfaceType)type : HardwareInterfaceType.Custom;
            return new ParseResult(resourceName, intf, (short)num, Native.FromZ(cls), Native.FromZ(expanded), Native.FromZ(alias));
        }

        public IVisaSession Open(string resourceName)
        {
            return Open(resourceName, AccessModes.None, 0, out _);
        }

        public IVisaSession Open(string resourceName, AccessModes accessModes, int timeoutMilliseconds)
        {
            return Open(resourceName, accessModes, timeoutMilliseconds, out _);
        }

        public IVisaSession Open(string resourceName, AccessModes accessModes, int timeoutMilliseconds, out ResourceOpenStatus openStatus)
        {
            if (string.IsNullOrEmpty(resourceName))
                throw new ArgumentException("an empty resource name", nameof(resourceName));
            if (_rm == 0)
                throw new ObjectDisposedException(nameof(ResourceManager));
            return OpenSession(resourceName, accessModes, timeoutMilliseconds, out openStatus);
        }

        /// <summary>
        /// A session with a default resource manager of its own: the GRM disposes the vendor managers
        /// it used as soon as Open returns, and in the C API closing a resource manager closes its
        /// sessions (VPP-4.3.6 RULE 17.3.5).
        /// </summary>
        internal static Session OpenSession(string resourceName, AccessModes accessModes, int timeoutMilliseconds, out ResourceOpenStatus openStatus)
        {
            Native.Load();
            uint mode = 0;
            if ((accessModes & AccessModes.ExclusiveLock) != 0)
                mode |= Native.VI_EXCLUSIVE_LOCK;
            if ((accessModes & AccessModes.LoadConfig) != 0)
                mode |= Native.VI_LOAD_CONFIG;
            uint rm, vi = 0;
            Native.Check(0, Native.viOpenDefaultRM(&rm));
            try
            {
                byte[] name = Native.Z(resourceName);
                int st;
                fixed (byte* n = name)
                    st = Native.viOpen(rm, n, mode, (uint)Math.Max(0, timeoutMilliseconds), &vi);
                Native.Check(rm, st);
                openStatus = st == Native.VI_SUCCESS_DEV_NPRESENT ? ResourceOpenStatus.DeviceNotResponding
                    : st == Native.VI_WARN_CONFIG_NLOADED ? ResourceOpenStatus.ConfigurationNotLoaded
                    : ResourceOpenStatus.Success;
                ushort type;
                Native.Check(vi, Native.viGetAttribute(vi, Native.VI_ATTR_INTF_TYPE, &type));
                byte* cls = stackalloc byte[Native.StringLength];
                Native.Check(vi, Native.viGetAttribute(vi, Native.VI_ATTR_RSRC_CLASS, cls));
                string rsrcClass = Native.FromZ(cls);
                Session s = Create(type, rsrcClass, rm, vi);
                vi = 0;
                rm = 0;
                return s;
            }
            finally
            {
                if (vi != 0)
                    Native.viClose(vi);
                if (rm != 0)
                    Native.viClose(rm);
            }
        }

        static Session Create(ushort type, string rsrcClass, uint rm, uint vi)
        {
            switch (type)
            {
                case Native.VI_INTF_GPIB:
                    if (rsrcClass == "INTFC")
                        return new GpibInterfaceSession(rm, vi);
                    return new GpibSession(rm, vi);
                case Native.VI_INTF_TCPIP:
                    if (rsrcClass == "SOCKET")
                        return new TcpipSocketSession(rm, vi);
                    return new TcpipSession(rm, vi);
                case Native.VI_INTF_USB:
                    return new UsbSession(rm, vi);
                case Native.VI_INTF_ASRL:
                    return new SerialSession(rm, vi);
                default:
                    throw new NativeVisaException(Native.VI_ERROR_NSUP_OPER, $"no VISA.NET session for interface type {type} class {rsrcClass}");
            }
        }

        public void Dispose()
        {
            uint rm = _rm;
            _rm = 0;
            if (rm != 0)
                Native.viClose(rm);
        }
    }
}
