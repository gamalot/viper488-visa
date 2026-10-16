using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using Ivi.Visa.Interop;
using Microsoft.Win32;
using Symbiotic.Visa;

namespace Symbiotic.VisaCom
{
    /// <summary>
    /// The vendor-specific resource manager (SRM) VISA COM's Global Resource Manager (VISA.GlobalRM) finds in
    /// HKCR\VISA.GlobalRM\VendorSpecificRMs and asks to find, parse and open resources (VPP-4.3.4 4.2).
    /// </summary>
    [ComVisible(true), ClassInterface(ClassInterfaceType.None), Guid(ClassId), ProgId(ProgIdText)]
    public sealed unsafe partial class ResourceManager : ComObject
    {
        /// <summary>The same GUID as the C library's id with the IVI VISA router (windows/register-visa.ps1): the
        /// router and VISA.GlobalRM share one conflict table, where a vendor has one id.</summary>
        public const string ClassId = "6B1D2A40-7E31-4C8A-9F52-3D8A0B7C1E64";
        const string ProgIdText = "Symbiotic.ResourceManager.1";
        const string ProgIdIndependent = "Symbiotic.ResourceManager";
        const string Title = "Viper488 Symbiotic VISA COM I/O Resource Manager";
        /// <summary>RULE 6.1.8's category of vendor-specific resource managers.</summary>
        const string SrmCategory = "{db8cbf21-d6d3-11d4-aa51-00a024ee30bd}";

        /// <summary>"vy" packed, as VISA.NET's; ours, not a VPP-9 assignment.</summary>
        public const short VendorId = 0x7679;
        public const string VendorName = "Viper488 Symbiotic";

        uint _rm;

        /// <summary>The C library in use (not a VISA COM property: for logs and scripts, as rm.visalib is in pyvisa).</summary>
        public static string NativeLibraryPath => Native.LibraryPath;

        uint Rm
        {
            get
            {
                if (_rm == 0)
                {
                    Native.Load();
                    uint rm;
                    Check(0, Native.viOpenDefaultRM(&rm));
                    _rm = rm;
                }
                return _rm;
            }
        }

        ~ResourceManager()
        {
            if (_rm != 0)
                Native.viClose(_rm);
        }

        public string SoftwareManufacturerName => VendorName;

        public short SoftwareManufacturerID => VendorId;

        public string Description => Title;

        public int ComponentVersion => ComObjects.VersionWord(GetType().Assembly.GetName().Version);

        public string ProgID => ProgIdText;

        public int SpecVersion
        {
            get
            {
                uint v = 0;
                Check(Rm, Native.viGetAttribute(Rm, Native.VI_ATTR_RSRC_SPEC_VERSION, &v));
                return (int)v;
            }
        }

        /// <summary>viFindRsrc and viFindNext to the end (RULE 4.2.8); nothing found is VI_ERROR_RSRC_NFOUND.</summary>
        public string[] FindRsrc(string expr)
        {
            var found = new List<string>();
            byte[] e = Native.Z(expr ?? "");
            byte* desc = stackalloc byte[Native.StringLength];
            uint list = 0, count = 0;
            fixed (byte* p = e)
                Check(Rm, Native.viFindRsrc(Rm, p, &list, &count, desc));
            try
            {
                found.Add(Native.FromZ(desc));
                for (uint i = 1; i < count; i++)
                {
                    Check(Rm, Native.viFindNext(list, desc));
                    found.Add(Native.FromZ(desc));
                }
            }
            finally
            {
                if (list != 0)
                    Native.viClose(list);
            }
            return found.ToArray();
        }

        /// <summary>The component for the resource's kind, opened with Init in the private domain (see Sessions).</summary>
        public IVisaSession Open(string ResourceName, AccessMode mode = AccessMode.NO_LOCK, int openTimeout = 2000, string OptionString = "")
        {
            Trace.Log($"ResourceManager.Open {ResourceName} mode {mode} timeout {openTimeout} options \"{OptionString}\"");
            int st = Sessions.Open(ResourceName, (int)mode, openTimeout, OptionString ?? "", out long unk, out string message);
            Trace.Log($"  the factory: 0x{st:X8} {message}");
            if (st < 0)
            {
                _lastStatus = st;
                throw Status.Error(st, message);
            }
            // The factory's pointer is a CCW from another AppDomain: what we get back is a plain RCW, and every
            // caller, managed or not, reaches the object through the [PreserveSig] twins.
            object o = Marshal.GetObjectForIUnknown((IntPtr)unk);
            Marshal.Release((IntPtr)unk);
            Check(0, st); // the open's completion code (VI_SUCCESS_DEV_NPRESENT ...) is Open's
            return (IVisaSession)o;
        }

        public void ParseRsrc(string ResourceName, ref short pInterfaceType, ref short pInterfaceNumber, ref string pSessionType)
        {
            string expanded = "", alias = "";
            ParseRsrcEx(ResourceName, ref pInterfaceType, ref pInterfaceNumber, ref pSessionType, ref expanded, ref alias);
        }

        /// <summary>viParseRsrcEx: no I/O (RULE 4.2.11), only the resource names this runtime has.</summary>
        public void ParseRsrcEx(string ResourceName, ref short pInterfaceType, ref short pInterfaceNumber, ref string pSessionType, ref string pUnaliasedExpandedResourceName, ref string pAliasIfExists)
        {
            byte[] name = Native.Z(ResourceName ?? "");
            ushort type, num;
            byte* cls = stackalloc byte[Native.StringLength];
            byte* expanded = stackalloc byte[Native.StringLength];
            byte* alias = stackalloc byte[Native.StringLength];
            fixed (byte* n = name)
                Check(Rm, Native.viParseRsrcEx(Rm, n, &type, &num, cls, expanded, alias));
            pInterfaceType = (short)type;
            pInterfaceNumber = (short)num;
            pSessionType = Native.FromZ(cls);
            pUnaliasedExpandedResourceName = Native.FromZ(expanded);
            pAliasIfExists = Native.FromZ(alias);
        }

        // ------------------------------------------------------------------------------------
        // Registration (regasm): what regasm writes, plus VPP-4.3.4 RULE 6.1.5's version-independent ProgID,
        // the VendorSpecificRMs entry the Global Resource Manager reads and RULE 6.1.8's category.

        [ComRegisterFunction]
        static void Register(Type t)
        {
            if (t != typeof(ResourceManager))
                return;
            string clsid = "{" + ClassId + "}";
            // The CLSID without braces: GlobMgr builds "CLSID\{" + name + "}" from each subkey's name; with braces it
            // finds nothing.
            Registry.ClassesRoot.DeleteSubKeyTree(@"VISA.GlobalRM\VendorSpecificRMs\" + clsid, false);
            using (var k = Registry.ClassesRoot.CreateSubKey(@"VISA.GlobalRM\VendorSpecificRMs\" + ClassId))
                k.SetValue("", Title);
            using (var k = Registry.ClassesRoot.CreateSubKey(@"CLSID\" + clsid + @"\Implemented Categories\" + SrmCategory)) { }
            using (var k = Registry.ClassesRoot.CreateSubKey(@"CLSID\" + clsid + @"\VersionIndependentProgID"))
                k.SetValue("", ProgIdIndependent);
            using (var k = Registry.ClassesRoot.CreateSubKey(ProgIdIndependent))
            {
                k.SetValue("", Title);
                using (var c = k.CreateSubKey("CLSID"))
                    c.SetValue("", clsid);
                using (var c = k.CreateSubKey("CurVer"))
                    c.SetValue("", ProgIdText);
            }
            using (var k = Registry.ClassesRoot.CreateSubKey(@"Component Categories\" + SrmCategory))
            {
                if (k.GetValue("409") == null)
                    k.SetValue("409", "VISA COM I/O Vendor-Specific Resource Manager Classes");
            }
        }

        [ComUnregisterFunction]
        static void Unregister(Type t)
        {
            if (t != typeof(ResourceManager))
                return;
            string clsid = "{" + ClassId + "}";
            Registry.ClassesRoot.DeleteSubKeyTree(@"VISA.GlobalRM\VendorSpecificRMs\" + ClassId, false);
            Registry.ClassesRoot.DeleteSubKeyTree(@"VISA.GlobalRM\VendorSpecificRMs\" + clsid, false);
            Registry.ClassesRoot.DeleteSubKeyTree(ProgIdIndependent, false);
            // What Register added under CLSID\{...}: RegAsm removes only its own, and leaves the key while anything
            // else is in it.
            Registry.ClassesRoot.DeleteSubKeyTree(@"CLSID\" + clsid + @"\Implemented Categories", false);
            Registry.ClassesRoot.DeleteSubKeyTree(@"CLSID\" + clsid + @"\VersionIndependentProgID", false);
        }
    }
}
