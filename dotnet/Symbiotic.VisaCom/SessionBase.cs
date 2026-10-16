using System;
using System.Collections.Generic;
using System.Globalization;
using System.Linq;
using System.Reflection;
using System.Runtime.InteropServices;
using Ivi.Visa.Interop;
using Symbiotic.Visa;

namespace Symbiotic.VisaCom
{
    /// <summary>
    /// What every VISA COM resource component has: IVisaSession and IEventManager, as public members with the
    /// interop assembly's signatures (a .NET caller in the same process calls them directly; COM callers come
    /// through the [PreserveSig] twins in Generated.cs). Each session opens its own default resource manager, as
    /// VISA.NET's do, and closes it with itself.
    /// </summary>
    [ComVisible(false)]
    public abstract unsafe class SessionBase : ComObject
    {
        uint _rm;
        uint _vi;
        bool _closed;
        string _options = "";

        protected abstract string ClassProgId { get; }

        internal uint Vi
        {
            get
            {
                if (_vi == 0)
                    throw Status.Error(Status.VI_ERROR_INV_SESSION, _closed ? "the session is closed" : "the session was never opened (Init)");
                return _vi;
            }
        }

        internal int Check(int status) => Check(_vi, status);

        // ------------------------------------------------------------------------------------
        // Attributes, read into an 8-byte word or a VI_FIND_BUFLEN string.

        internal ulong Raw(uint attribute)
        {
            byte* buf = stackalloc byte[Native.StringLength];
            *(ulong*)buf = 0;
            Check(Native.viGetAttribute(Vi, attribute, buf));
            return *(ulong*)buf;
        }

        internal string Str(uint attribute)
        {
            byte* buf = stackalloc byte[Native.StringLength];
            buf[0] = 0;
            Check(Native.viGetAttribute(Vi, attribute, buf));
            return Native.FromZ(buf);
        }

        internal bool Bool(uint attribute) => (ushort)Raw(attribute) != 0;
        internal short I16(uint attribute) => (short)Raw(attribute);
        internal int I32(uint attribute) => (int)Raw(attribute);
        internal byte U8(uint attribute) => (byte)Raw(attribute);
        internal LineState Line(uint attribute) => (LineState)(short)Raw(attribute);

        internal void Put(uint attribute, ulong value) => Check(Native.viSetAttribute(Vi, attribute, (nuint)value));
        internal void Put(uint attribute, bool value) => Put(attribute, value ? 1UL : 0UL);

        // ------------------------------------------------------------------------------------
        // IVisaSession

        public int ComponentVersion => ComObjects.VersionWord(GetType().Assembly.GetName().Version);

        public int SpecVersion => (int)(uint)Raw(Native.VI_ATTR_RSRC_SPEC_VERSION);

        public string HardwareInterfaceName => Str(Native.VI_ATTR_INTF_INST_NAME);

        public short HardwareInterfaceNumber => I16(Native.VI_ATTR_INTF_NUM);

        public short HardwareInterfaceType => I16(Native.VI_ATTR_INTF_TYPE);

        public AccessMode LockState => (AccessMode)I32(Native.VI_ATTR_RSRC_LOCK_STATE);

        public string OptionString => _options;

        public string ProgID => ClassProgId;

        public string ResourceName => Str(Native.VI_ATTR_RSRC_NAME);

        public string SessionType => Str(Native.VI_ATTR_RSRC_CLASS);

        public short SoftwareManufacturerID => ResourceManager.VendorId;

        public string SoftwareManufacturerName => ResourceManager.VendorName;

        /// <summary>Milliseconds; VI_TMO_INFINITE is -1 in a long.</summary>
        public int Timeout
        {
            get => I32(Native.VI_ATTR_TMO_VALUE);
            set => Put(Native.VI_ATTR_TMO_VALUE, (uint)value);
        }

        public int LastStatus => _lastStatus;

        /// <summary>Any VISA attribute, as a VARIANT of its type (the type table comes from pyvisa's, AttributeTypes.cs).
        /// VPP-4.3.4's COM types: 16-bit values as short, 32-bit as long, booleans as VARIANT_BOOL.</summary>
        public object GetAttribute(int attribute)
        {
            uint a = (uint)attribute;
            if (!AttributeTypes.Kinds.TryGetValue(a, out AttrKind kind))
                kind = AttrKind.U32;
            switch (kind)
            {
                case AttrKind.Str: return Str(a);
                case AttrKind.Bool: return Bool(a);
                case AttrKind.U8: return U8(a);
                case AttrKind.I16:
                case AttrKind.U16: return I16(a);
                case AttrKind.U64: return (long)Raw(a);
                case AttrKind.Ptr: throw Status.Error(Native.VI_ERROR_NSUP_ATTR, $"attribute 0x{a:X8} is a buffer, which a VARIANT cannot carry");
                default: return I32(a);
            }
        }

        public void SetAttribute(int attribute, object attrState)
        {
            uint a = (uint)attribute;
            if (attrState is string)
                throw Status.Error(Status.VI_ERROR_NSUP_ATTR_STATE, $"attribute 0x{a:X8}: no attribute here takes a string");
            ulong v;
            try
            {
                v = attrState is bool b ? (b ? 1UL : 0UL) : unchecked((ulong)Convert.ToInt64(attrState, CultureInfo.InvariantCulture));
            }
            catch (Exception)
            {
                throw Status.Error(Status.VI_ERROR_NSUP_ATTR_STATE, $"attribute 0x{a:X8}: {attrState} is not a number");
            }
            if (AttributeTypes.Kinds.TryGetValue(a, out AttrKind kind))
            {
                switch (kind)
                {
                    case AttrKind.U8: v &= 0xFF; break;
                    case AttrKind.I16: case AttrKind.U16: case AttrKind.Bool: v &= 0xFFFF; break;
                    case AttrKind.I32: case AttrKind.U32: v &= 0xFFFFFFFF; break;
                }
            }
            Put(a, v);
        }

        public string LockRsrc(AccessMode type = AccessMode.EXCLUSIVE_LOCK, int lockTimeout = 2000, string requestedKey = "")
        {
            byte* access = stackalloc byte[Native.StringLength];
            access[0] = 0;
            byte[] key = type == AccessMode.SHARED_LOCK && !string.IsNullOrEmpty(requestedKey) ? Native.Z(requestedKey) : null;
            fixed (byte* k = key)
                Check(Native.viLock(Vi, (uint)type, (uint)lockTimeout, k, type == AccessMode.SHARED_LOCK ? access : null));
            return Native.FromZ(access);
        }

        public void UnlockRsrc() => Check(Native.viUnlock(Vi));

        /// <summary>viOpen, then the option string's properties (VPP-4.3.4 3.2.2.1).</summary>
        public void Init(string ResourceName, AccessMode mode = AccessMode.NO_LOCK, int initTimeout = 2000, string OptionString = "")
        {
            if (_vi != 0 || _closed)
                throw Status.Error(Status.VI_ERROR_INV_SETUP, _closed ? "Init on a closed session" : "Init on a session already open");
            var options = ParseOptions(OptionString ?? "");
            Native.Load();
            uint rm, vi = 0;
            Check(0, Native.viOpenDefaultRM(&rm));
            try
            {
                byte[] name = Native.Z(ResourceName);
                int st;
                fixed (byte* n = name)
                    st = Native.viOpen(rm, n, (uint)mode, (uint)Math.Max(0, initTimeout), &vi);
                Check(rm, st);
                ushort type;
                Check(vi, Native.viGetAttribute(vi, Native.VI_ATTR_INTF_TYPE, &type));
                byte* cls = stackalloc byte[Native.StringLength];
                Check(vi, Native.viGetAttribute(vi, Native.VI_ATTR_RSRC_CLASS, cls));
                if (!Accepts(type, Native.FromZ(cls)))
                    throw Status.Error(Native.VI_ERROR_NSUP_OPER, $"{ResourceName} is a {Native.FromZ(cls)} resource of interface type {type}, not this component's ({ClassProgId})");
                _rm = rm;
                _vi = vi;
                rm = vi = 0;
                _options = OptionString ?? "";
                int completion = st;
                try
                {
                    ApplyOptions(options);
                }
                catch (Exception)
                {
                    // Not left open (and perhaps locked) until the collector gets to it: a failed Init opens nothing.
                    Close();
                    _closed = false;
                    throw;
                }
                Check(_vi, completion); // the open's completion code (VI_SUCCESS_DEV_NPRESENT ...) is the call's
            }
            finally
            {
                if (vi != 0)
                    Native.viClose(vi);
                if (rm != 0)
                    Native.viClose(rm);
            }
        }

        /// <summary>Which resources this component serves (RULE 3.2.16: Init opens only its own kind).</summary>
        protected abstract bool Accepts(ushort interfaceType, string resourceClass);

        public void Close()
        {
            uint vi = _vi, rm = _rm;
            _vi = _rm = 0;
            _closed = true;
            lock (_handlers)
                _handlers.Clear();
            if (vi != 0)
                Native.viClose(vi);
            if (rm != 0)
                Native.viClose(rm);
            FreeAsyncBuffers();
            OnClosed();
        }

        protected virtual void OnClosed() { }

        ~SessionBase()
        {
            if (_vi != 0 || _rm != 0)
                Close();
        }

        // ------------------------------------------------------------------------------------
        // The option string: "Name = value; ..." with the settable COM properties of this component's
        // interfaces (RULE 3.2.9), names and enumeration values in any case.

        static List<KeyValuePair<string, string>> ParseOptions(string s)
        {
            var pairs = new List<KeyValuePair<string, string>>();
            foreach (string part in s.Split(';'))
            {
                if (part.Trim().Length == 0)
                    continue;
                int eq = part.IndexOf('=');
                string name = eq < 0 ? "" : part.Substring(0, eq).Trim();
                string value = eq < 0 ? "" : part.Substring(eq + 1).Trim();
                if (name.Length == 0 || value.Length == 0 || !(char.IsLetter(name[0]) || name[0] == '_') || !name.All(c => char.IsLetterOrDigit(c) || c == '_'))
                    throw Status.Error(Status.E_INVALIDARG, $"a malformed option string: \"{s}\"");
                pairs.Add(new KeyValuePair<string, string>(name, value));
            }
            return pairs;
        }

        void ApplyOptions(List<KeyValuePair<string, string>> options)
        {
            foreach (var o in options)
            {
                PropertyInfo prop = null;
                foreach (Type i in GetType().GetInterfaces().Where(t => t.Namespace == "Ivi.Visa.Interop"))
                {
                    prop = i.GetProperties().FirstOrDefault(p => string.Equals(p.Name, o.Key, StringComparison.OrdinalIgnoreCase));
                    if (prop != null && prop.CanWrite)
                        break;
                }
                if (prop == null)
                    throw Status.Error(Native.VI_ERROR_NSUP_ATTR, $"option {o.Key}: no such property");
                if (!prop.CanWrite)
                    throw Status.Error(Status.VI_ERROR_ATTR_READONLY, $"option {o.Key}: the property is read-only");
                prop.SetValue(this, OptionValue(prop.PropertyType, o.Key, o.Value));
            }
        }

        static object OptionValue(Type t, string name, string text)
        {
            if (TryNumber(text, out long n))
            {
                if (t == typeof(bool)) return n != 0;
                if (t.IsEnum) return Enum.ToObject(t, n);
                return Convert.ChangeType(n, t, CultureInfo.InvariantCulture);
            }
            if (t == typeof(bool))
            {
                if (text.Equals("TRUE", StringComparison.OrdinalIgnoreCase)) return true;
                if (text.Equals("FALSE", StringComparison.OrdinalIgnoreCase)) return false;
            }
            if (t.IsEnum)
            {
                string e = Enum.GetNames(t).FirstOrDefault(x => x.Equals(text, StringComparison.OrdinalIgnoreCase));
                if (e != null) return Enum.Parse(t, e);
            }
            throw Status.Error(Status.E_INVALIDARG, $"option {name}: \"{text}\" is not a value it takes");
        }

        static bool TryNumber(string s, out long n)
        {
            if (s.StartsWith("0x", StringComparison.OrdinalIgnoreCase))
                return long.TryParse(s.Substring(2), NumberStyles.HexNumber, CultureInfo.InvariantCulture, out n);
            return long.TryParse(s, NumberStyles.Integer, CultureInfo.InvariantCulture, out n);
        }

        // ------------------------------------------------------------------------------------
        // IEventManager

        public int MaximumQueueLength
        {
            get => I32(Native.VI_ATTR_MAX_QUEUE_LENGTH);
            set => Put(Native.VI_ATTR_MAX_QUEUE_LENGTH, (uint)value);
        }

        static uint TypeCode(EventType type, int customEventType) =>
            type == EventType.CUSTOM_EVENT_TYPE ? (uint)customEventType : (uint)type;

        public void EnableEvent(EventType type, EventMechanism mech, int customEventType = 0) =>
            Check(Native.viEnableEvent(Vi, TypeCode(type, customEventType), (ushort)mech, 0));

        public void DisableEvent(EventType type = EventType.ALL_ENABLED_EVENTS, EventMechanism mech = EventMechanism.EVENT_ALL_MECH, int customEventType = 0) =>
            Check(Native.viDisableEvent(Vi, TypeCode(type, customEventType), (ushort)mech));

        public void DiscardEvents(EventType type = EventType.ALL_ENABLED_EVENTS, EventMechanism mech = EventMechanism.EVENT_ALL_MECH, int customEventType = 0) =>
            Check(Native.viDiscardEvents(Vi, TypeCode(type, customEventType), (ushort)mech));

        public IEvent WaitOnEvent(int waitTimeout, EventType type = EventType.ALL_ENABLED_EVENTS, int customEventType = 0)
        {
            uint outType, context;
            Check(Native.viWaitOnEvent(Vi, TypeCode(type, customEventType), (uint)waitTimeout, &outType, &context));
            return Event.Queued(this, outType, context);
        }

        [UnmanagedFunctionPointer(CallingConvention.StdCall)]
        delegate int NativeHandler(uint vi, uint eventType, uint context, IntPtr userHandle);

        /// <summary>One entry per InstallHandler: the C side calls back with the entry's key as its user handle.</summary>
        sealed class Installed
        {
            public uint Type;
            public IEventHandler Handler;
            public int UserHandle;
        }

        readonly Dictionary<int, Installed> _handlers = new Dictionary<int, Installed>();
        int _nextKey;
        NativeHandler _native;
        IntPtr _nativePtr;

        public void InstallHandler(EventType type, IEventHandler handler, int userHandle = 0, int customEventType = 0)
        {
            if (handler == null)
                throw Status.Error(Status.VI_ERROR_INV_PARAMETER, "no handler");
            uint t = TypeCode(type, customEventType);
            int key;
            lock (_handlers)
            {
                if (_native == null)
                {
                    _native = OnNativeEvent;
                    _nativePtr = Marshal.GetFunctionPointerForDelegate(_native);
                }
                key = ++_nextKey;
                _handlers[key] = new Installed { Type = t, Handler = handler, UserHandle = userHandle };
            }
            int st = Native.viInstallHandler(Vi, t, _nativePtr, (IntPtr)key);
            if (st < 0)
            {
                lock (_handlers)
                    _handlers.Remove(key);
            }
            Check(st);
        }

        /// <summary>The handlers of this type with this user handle (VI_ANY_HNDLR, 0, meaning all of the type's).</summary>
        public void UninstallHandler(EventType type, int userHandle = 0, int customEventType = 0)
        {
            uint t = TypeCode(type, customEventType);
            List<int> keys;
            lock (_handlers)
                keys = _handlers.Where(h => h.Value.Type == t && (userHandle == 0 || h.Value.UserHandle == userHandle)).Select(h => h.Key).ToList();
            if (keys.Count == 0)
                throw Status.Error(Status.VI_ERROR_HNDLR_NINSTALLED, "no such handler installed");
            foreach (int k in keys)
            {
                Check(Native.viUninstallHandler(Vi, t, _nativePtr, (IntPtr)k));
                lock (_handlers)
                    _handlers.Remove(k);
            }
        }

        int OnNativeEvent(uint vi, uint eventType, uint context, IntPtr userHandle)
        {
            try
            {
                Installed h;
                lock (_handlers)
                    _handlers.TryGetValue((int)userHandle, out h);
                if (h != null)
                    h.Handler.HandleEvent((IEventManager)this, Event.Handled(this, eventType, context), h.UserHandle);
            }
            catch (Exception)
            {
                // Nothing may unwind into the C library's thread.
            }
            return Native.VI_SUCCESS;
        }

        // ------------------------------------------------------------------------------------
        // Asynchronous reads: the C API finishes them before viReadAsync returns and posts the completion
        // event with the buffer's address; the buffer lives in unmanaged memory, kept here by job. One
        // completion may reach a queue and every handler, at any time after, so no one of them can say the
        // buffer is done with: an event copies from it through ReadAsyncBuffer, by job, under the lock,
        // and the oldest buffers past AsyncBuffersKept are let go (a completion that comes for one of
        // those has no bytes, never freed memory), so a program that never collects completions does not
        // grow without end. The rest go when the session closes.

        const int AsyncBuffersKept = 32;
        readonly Dictionary<uint, IntPtr> _asyncBuffers = new Dictionary<uint, IntPtr>();
        readonly Queue<uint> _asyncOrder = new Queue<uint>();
        // Buffers whose viReadAsync/viWriteAsync has not returned: its handlers run inside the call, before the
        // job number is known here, and the buffer is alive until the call returns.
        readonly HashSet<IntPtr> _asyncInFlight = new HashSet<IntPtr>();

        internal void AsyncStarting(IntPtr buffer)
        {
            lock (_asyncBuffers)
                _asyncInFlight.Add(buffer);
        }

        /// <summary>The call for `buffer` returned: kept by `job`, or (it failed, job 0) freed.</summary>
        internal void KeepAsyncBuffer(uint job, IntPtr buffer)
        {
            lock (_asyncBuffers)
            {
                _asyncInFlight.Remove(buffer);
                if (job == 0)
                {
                    Marshal.FreeHGlobal(buffer);
                    return;
                }
                if (_asyncBuffers.TryGetValue(job, out IntPtr old))
                    Marshal.FreeHGlobal(old);
                else
                    _asyncOrder.Enqueue(job);
                _asyncBuffers[job] = buffer;
                while (_asyncOrder.Count > AsyncBuffersKept)
                {
                    uint gone = _asyncOrder.Dequeue();
                    if (_asyncBuffers.TryGetValue(gone, out IntPtr p))
                    {
                        Marshal.FreeHGlobal(p);
                        _asyncBuffers.Remove(gone);
                    }
                }
            }
        }

        /// <summary>The first `count` bytes of job `job`'s buffer, if it is still kept, or of `address` (the event's
        /// VI_ATTR_BUFFER) if that is a buffer whose call has not returned (a handler inside it); else none.</summary>
        internal byte[] ReadAsyncBuffer(uint job, IntPtr address, int count)
        {
            lock (_asyncBuffers)
            {
                if (count <= 0)
                    return new byte[0];
                if (!_asyncBuffers.TryGetValue(job, out IntPtr p))
                {
                    if (address == IntPtr.Zero || !_asyncInFlight.Contains(address))
                        return new byte[0];
                    p = address;
                }
                var b = new byte[count];
                Marshal.Copy(p, b, 0, count);
                return b;
            }
        }

        internal void FreeAsyncBuffers()
        {
            lock (_asyncBuffers)
            {
                foreach (IntPtr p in _asyncBuffers.Values)
                    Marshal.FreeHGlobal(p);
                _asyncBuffers.Clear();
                _asyncOrder.Clear();
            }
        }
    }
}
