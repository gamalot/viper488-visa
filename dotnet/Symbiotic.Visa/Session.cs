using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Threading;
using Ivi.Visa;

namespace Symbiotic.Visa
{
    /// <summary>
    /// What every session has: attributes, locks, events. Each session owns the default resource
    /// manager it was opened from, and closes it with itself (see ResourceManager.OpenSession).
    /// </summary>
    public abstract unsafe class Session : IVisaSession, INativeVisaSession
    {
        [UnmanagedFunctionPointer(CallingConvention.StdCall)]
        delegate int Handler(uint vi, uint eventType, uint context, IntPtr userHandle);

        uint _rm;
        uint _vi;
        readonly SynchronizationContext _context;
        readonly object _events = new object();
        /// <summary>One native handler for the session, held so the collector never takes it while the C side can call it.</summary>
        Handler _handler;
        IntPtr _handlerPtr;
        /// <summary>What each event type installed with the handler mechanism raises.</summary>
        readonly Dictionary<uint, Action<VisaEventArgs>> _raise = new Dictionary<uint, Action<VisaEventArgs>>();

        internal Session(uint rm, uint vi)
        {
            _rm = rm;
            _vi = vi;
            _context = SynchronizationContext.Current;
        }

        internal uint Vi
        {
            get
            {
                if (_vi == 0)
                    throw new ObjectDisposedException(GetType().Name);
                return _vi;
            }
        }

        internal int Check(int status) => Native.Check(_vi, status);

        // ------------------------------------------------------------------------------------
        // Attributes. Numbers are read into an 8-byte word (the C side writes only its own width);
        // strings into VI_FIND_BUFLEN bytes.

        internal ulong GetRaw(uint attribute)
        {
            byte* buf = stackalloc byte[Native.StringLength];
            *(ulong*)buf = 0;
            Check(Native.viGetAttribute(Vi, attribute, buf));
            return *(ulong*)buf;
        }

        internal string GetString(uint attribute)
        {
            byte* buf = stackalloc byte[Native.StringLength];
            buf[0] = 0;
            Check(Native.viGetAttribute(Vi, attribute, buf));
            return Native.FromZ(buf);
        }

        internal bool GetBool(uint attribute) => (ushort)GetRaw(attribute) != 0;
        internal byte GetU8(uint attribute) => (byte)GetRaw(attribute);
        internal short GetI16(uint attribute) => (short)GetRaw(attribute);
        internal ushort GetU16(uint attribute) => (ushort)GetRaw(attribute);
        internal int GetI32(uint attribute) => (int)GetRaw(attribute);
        internal uint GetU32(uint attribute) => (uint)GetRaw(attribute);
        internal LineState GetLine(uint attribute) => ToLine(GetI16(attribute));

        internal void Set(uint attribute, ulong value) => Check(Native.viSetAttribute(Vi, attribute, (nuint)value));
        internal void SetBool(uint attribute, bool value) => Set(attribute, value ? 1UL : 0UL);

        internal static LineState ToLine(short state) =>
            state == 1 ? LineState.Asserted : state == 0 ? LineState.Unasserted : LineState.Unknown;

        /// <summary>VPP-4.3's version word: major in the top 12 bits, minor in the next 12, sub-minor in the low 8
        /// (VI_SPEC_VERSION 0x00700100 is 7.1.0).</summary>
        internal static Version FromVersionWord(uint v) => new Version((int)(v >> 20), (int)((v >> 8) & 0xFFF), (int)(v & 0xFF));

        // ------------------------------------------------------------------------------------
        // IVisaSession

        public int EventQueueCapacity
        {
            get => GetI32(Native.VI_ATTR_MAX_QUEUE_LENGTH);
            set => Set(Native.VI_ATTR_MAX_QUEUE_LENGTH, (uint)value);
        }

        public string HardwareInterfaceName => GetString(Native.VI_ATTR_INTF_INST_NAME);

        public short HardwareInterfaceNumber => GetI16(Native.VI_ATTR_INTF_NUM);

        public HardwareInterfaceType HardwareInterfaceType
        {
            get
            {
                ushort t = GetU16(Native.VI_ATTR_INTF_TYPE);
                return t <= 7 ? (HardwareInterfaceType)t : HardwareInterfaceType.Custom;
            }
        }

        public string ResourceClass => GetString(Native.VI_ATTR_RSRC_CLASS);

        public Version ResourceImplementationVersion => FromVersionWord(GetU32(Native.VI_ATTR_RSRC_IMPL_VERSION));

        public ResourceLockState ResourceLockState
        {
            get
            {
                uint s = GetU32(Native.VI_ATTR_RSRC_LOCK_STATE);
                return s == Native.VI_EXCLUSIVE_LOCK ? ResourceLockState.ExclusiveLock
                    : s == Native.VI_SHARED_LOCK ? ResourceLockState.SharedLock : ResourceLockState.NoLock;
            }
        }

        public short ResourceManufacturerId => ResourceManager.VendorId;

        public string ResourceManufacturerName => GetString(Native.VI_ATTR_RSRC_MANF_NAME);

        public string ResourceName => GetString(Native.VI_ATTR_RSRC_NAME);

        public Version ResourceSpecificationVersion => FromVersionWord(GetU32(Native.VI_ATTR_RSRC_SPEC_VERSION));

        /// <summary>Raise events through the SynchronizationContext current when the session was opened.</summary>
        public bool SynchronizeCallbacks { get; set; }

        public int TimeoutMilliseconds
        {
            get
            {
                uint t = GetU32(Native.VI_ATTR_TMO_VALUE);
                return t == Native.VI_TMO_INFINITE ? VisaConstants.InfiniteTimeout : (int)Math.Min(t, int.MaxValue);
            }
            set => Set(Native.VI_ATTR_TMO_VALUE, value < 0 ? Native.VI_TMO_INFINITE : (uint)value);
        }

        static uint Ms(TimeSpan t) => t < TimeSpan.Zero ? Native.VI_TMO_INFINITE : (uint)Math.Min(t.TotalMilliseconds, uint.MaxValue - 1);
        static uint Ms(int t) => t < 0 ? Native.VI_TMO_INFINITE : (uint)t;

        public void LockResource() => LockResource(TimeoutMilliseconds);

        public void LockResource(TimeSpan timeout) => Lock(Native.VI_EXCLUSIVE_LOCK, Ms(timeout), null);

        public void LockResource(int timeoutMilliseconds) => Lock(Native.VI_EXCLUSIVE_LOCK, Ms(timeoutMilliseconds), null);

        public string LockResource(TimeSpan timeout, string sharedKey) => Lock(Native.VI_SHARED_LOCK, Ms(timeout), sharedKey);

        public string LockResource(int timeoutMilliseconds, string sharedKey) => Lock(Native.VI_SHARED_LOCK, Ms(timeoutMilliseconds), sharedKey);

        string Lock(uint type, uint timeout, string key)
        {
            byte* access = stackalloc byte[Native.StringLength];
            access[0] = 0;
            if (type == Native.VI_SHARED_LOCK)
            {
                byte[] k = string.IsNullOrEmpty(key) ? null : Native.Z(key);
                fixed (byte* kp = k)
                    Check(Native.viLock(Vi, type, timeout, kp, access));
                return Native.FromZ(access);
            }
            Check(Native.viLock(Vi, type, timeout, null, null));
            return null;
        }

        public void UnlockResource() => Check(Native.viUnlock(Vi));

        // ------------------------------------------------------------------------------------
        // Events through the queue: EnableEvent, WaitOnEvent and the rest.

        public void EnableEvent(EventType eventType) => EnableEvent((int)eventType);

        public void DisableEvent(EventType eventType) => DisableEvent((int)eventType);

        public void DiscardEvents(EventType eventType) => DiscardEvents((int)eventType);

        public VisaEventArgs WaitOnEvent(EventType eventType) => WaitOnEvent(eventType, TimeoutMilliseconds, out _);

        public VisaEventArgs WaitOnEvent(EventType eventType, out EventQueueStatus status) => WaitOnEvent(eventType, TimeoutMilliseconds, out status);

        public VisaEventArgs WaitOnEvent(EventType eventType, int timeoutMilliseconds) => WaitOnEvent(eventType, timeoutMilliseconds, out _);

        public VisaEventArgs WaitOnEvent(EventType eventType, TimeSpan timeout) => WaitOnEvent(eventType, (int)Math.Min(timeout.TotalMilliseconds, int.MaxValue), out _);

        public VisaEventArgs WaitOnEvent(EventType eventType, TimeSpan timeout, out EventQueueStatus status) =>
            WaitOnEvent(eventType, (int)Math.Min(timeout.TotalMilliseconds, int.MaxValue), out status);

        public VisaEventArgs WaitOnEvent(EventType eventType, int timeoutMilliseconds, out EventQueueStatus status)
        {
            using (var e = (NativeEventArgs)WaitOnEvent((int)eventType, timeoutMilliseconds, out status))
                return e.EventArgs;
        }

        public void EnableEvent(int eventType) => Check(Native.viEnableEvent(Vi, (uint)eventType, Native.VI_QUEUE, 0));

        public void DisableEvent(int eventType) => Check(Native.viDisableEvent(Vi, (uint)eventType, Native.VI_QUEUE));

        public void DiscardEvents(int eventType) => Check(Native.viDiscardEvents(Vi, (uint)eventType, Native.VI_QUEUE));

        public INativeVisaEventArgs WaitOnEvent(int eventType) => WaitOnEvent(eventType, TimeoutMilliseconds, out _);

        public INativeVisaEventArgs WaitOnEvent(int eventType, out EventQueueStatus status) => WaitOnEvent(eventType, TimeoutMilliseconds, out status);

        public INativeVisaEventArgs WaitOnEvent(int eventType, int timeoutMilliseconds) => WaitOnEvent(eventType, timeoutMilliseconds, out _);

        public INativeVisaEventArgs WaitOnEvent(int eventType, TimeSpan timeout) => WaitOnEvent(eventType, (int)Math.Min(timeout.TotalMilliseconds, int.MaxValue), out _);

        public INativeVisaEventArgs WaitOnEvent(int eventType, TimeSpan timeout, out EventQueueStatus status) =>
            WaitOnEvent(eventType, (int)Math.Min(timeout.TotalMilliseconds, int.MaxValue), out status);

        public INativeVisaEventArgs WaitOnEvent(int eventType, int timeoutMilliseconds, out EventQueueStatus status)
        {
            uint type, context;
            int st = Check(Native.viWaitOnEvent(Vi, (uint)eventType, Ms(timeoutMilliseconds), &type, &context));
            status = st == Native.VI_SUCCESS_QUEUE_NEMPTY ? EventQueueStatus.NotEmpty : EventQueueStatus.Empty;
            return new NativeEventArgs(context, EventArgsOf(type, context));
        }

        /// <summary>The .NET description of a C event; the context is read, not closed.</summary>
        internal VisaEventArgs EventArgsOf(uint type, uint context)
        {
            switch (type)
            {
                case Native.VI_EVENT_SERVICE_REQ:
                    return new VisaEventArgs(EventType.ServiceRequest);
                case Native.VI_EVENT_CLEAR:
                    return new VisaEventArgs(EventType.Clear);
                case Native.VI_EVENT_TRIG:
                    return new VisaEventArgs(EventType.Trigger);
                case Native.VI_EVENT_GPIB_TALK:
                    return new VisaEventArgs(EventType.GpibTalk);
                case Native.VI_EVENT_GPIB_LISTEN:
                    return new VisaEventArgs(EventType.GpibListen);
                case Native.VI_EVENT_GPIB_CIC:
                    return new GpibControllerInChargeEventArgs(true);
                default:
                    return new VisaEventArgs(unchecked((int)type));
            }
        }

        // ------------------------------------------------------------------------------------
        // Events through handlers: the .NET events. The first subscriber installs the handler and
        // enables VI_HNDLR for its type; the last one to leave disables and uninstalls it.

        internal void Subscribe(uint eventType, Action<VisaEventArgs> raise)
        {
            lock (_events)
            {
                if (_handler == null)
                {
                    _handler = OnNativeEvent;
                    _handlerPtr = Marshal.GetFunctionPointerForDelegate(_handler);
                }
                Check(Native.viInstallHandler(Vi, eventType, _handlerPtr, IntPtr.Zero));
                int st = Native.viEnableEvent(Vi, eventType, Native.VI_HNDLR, 0);
                if (st < 0)
                {
                    Native.viUninstallHandler(Vi, eventType, _handlerPtr, IntPtr.Zero);
                    Check(st);
                }
                _raise[eventType] = raise;
            }
        }

        internal void Unsubscribe(uint eventType)
        {
            lock (_events)
            {
                if (!_raise.Remove(eventType) || _vi == 0)
                    return;
                Native.viDisableEvent(_vi, eventType, Native.VI_HNDLR);
                Native.viUninstallHandler(_vi, eventType, _handlerPtr, IntPtr.Zero);
            }
        }

        int OnNativeEvent(uint vi, uint eventType, uint context, IntPtr userHandle)
        {
            try
            {
                Action<VisaEventArgs> raise;
                lock (_events)
                    _raise.TryGetValue(eventType, out raise);
                if (raise == null)
                    return Native.VI_SUCCESS;
                // Read the context now: the C side closes it when this returns.
                VisaEventArgs args = EventArgsOf(eventType, context);
                SynchronizationContext sc = SynchronizeCallbacks ? _context : null;
                if (sc != null)
                    sc.Post(_ => raise(args), null);
                else
                    raise(args);
            }
            catch (Exception)
            {
                // An exception must not unwind into the C library's thread.
            }
            return Native.VI_SUCCESS;
        }

        /// <summary>A .NET event backed by one VISA event type: subscribe on the first handler, unsubscribe after the last.</summary>
        internal sealed class NativeEvent<T> where T : VisaEventArgs
        {
            readonly Session _session;
            readonly uint _type;
            EventHandler<T> _handlers;

            public NativeEvent(Session session, uint type)
            {
                _session = session;
                _type = type;
            }

            public void Add(EventHandler<T> h)
            {
                lock (this)
                {
                    if (_handlers == null)
                        _session.Subscribe(_type, a => _handlers?.Invoke(_session, (T)a));
                    _handlers += h;
                }
            }

            public void Remove(EventHandler<T> h)
            {
                lock (this)
                {
                    if (_handlers == null)
                        return;
                    _handlers -= h;
                    if (_handlers == null)
                        _session.Unsubscribe(_type);
                }
            }
        }

        // ------------------------------------------------------------------------------------
        // INativeVisaSession

        public int Handle => (int)Vi;

        public byte GetAttributeByte(NativeVisaAttribute attribute) => GetAttributeByte((int)attribute);
        public byte GetAttributeByte(int attribute) => GetU8((uint)attribute);
        public short GetAttributeInt16(NativeVisaAttribute attribute) => GetAttributeInt16((int)attribute);
        public short GetAttributeInt16(int attribute) => GetI16((uint)attribute);
        public int GetAttributeInt32(NativeVisaAttribute attribute) => GetAttributeInt32((int)attribute);
        public int GetAttributeInt32(int attribute) => GetI32((uint)attribute);
        public long GetAttributeInt64(NativeVisaAttribute attribute) => GetAttributeInt64((int)attribute);
        public long GetAttributeInt64(int attribute) => (long)GetRaw((uint)attribute);
        public bool GetAttributeBoolean(NativeVisaAttribute attribute) => GetAttributeBoolean((int)attribute);
        public bool GetAttributeBoolean(int attribute) => GetBool((uint)attribute);
        public string GetAttributeString(NativeVisaAttribute attribute) => GetAttributeString((int)attribute);
        public string GetAttributeString(int attribute) => GetString((uint)attribute);

        public void SetAttributeByte(NativeVisaAttribute attribute, byte value) => SetAttributeByte((int)attribute, value);
        public void SetAttributeByte(int attribute, byte value) => Set((uint)attribute, value);
        public void SetAttributeInt16(NativeVisaAttribute attribute, short value) => SetAttributeInt16((int)attribute, value);
        public void SetAttributeInt16(int attribute, short value) => Set((uint)attribute, (ushort)value);
        public void SetAttributeInt32(NativeVisaAttribute attribute, int value) => SetAttributeInt32((int)attribute, value);
        public void SetAttributeInt32(int attribute, int value) => Set((uint)attribute, (uint)value);
        public void SetAttributeInt64(NativeVisaAttribute attribute, long value) => SetAttributeInt64((int)attribute, value);
        public void SetAttributeInt64(int attribute, long value) => Set((uint)attribute, (ulong)value);
        public void SetAttributeBoolean(NativeVisaAttribute attribute, bool value) => SetAttributeBoolean((int)attribute, value);
        public void SetAttributeBoolean(int attribute, bool value) => SetBool((uint)attribute, value);
        public void SetAttributeString(NativeVisaAttribute attribute, string value) => SetAttributeString((int)attribute, value);

        /// <summary>No attribute this runtime has is a string that can be set.</summary>
        public void SetAttributeString(int attribute, string value) =>
            throw new NativeVisaException(Native.VI_ERROR_NSUP_ATTR, Native.Describe(_vi, Native.VI_ERROR_NSUP_ATTR));

        // ------------------------------------------------------------------------------------

        public override string ToString()
        {
            try { return ResourceName; } catch (Exception) { return base.ToString(); }
        }

        protected virtual void OnClosing() { }

        public void Dispose()
        {
            Dispose(true);
            GC.SuppressFinalize(this);
        }

        protected virtual void Dispose(bool disposing)
        {
            if (disposing)
                OnClosing();
            uint vi, rm;
            lock (_events)
            {
                vi = _vi;
                rm = _rm;
                _vi = 0;
                _rm = 0;
                _raise.Clear();
            }
            // Closing the session uninstalls its handlers; the delegate stays referenced until then.
            if (vi != 0)
                Native.viClose(vi);
            if (rm != 0)
                Native.viClose(rm);
            GC.KeepAlive(_handler);
        }

        ~Session()
        {
            Dispose(false);
        }
    }

    /// <summary>An event from WaitOnEvent with its C context still open for its attributes.</summary>
    internal sealed unsafe class NativeEventArgs : INativeVisaEventArgs
    {
        uint _context;

        public NativeEventArgs(uint context, VisaEventArgs args)
        {
            _context = context;
            EventArgs = args;
        }

        public VisaEventArgs EventArgs { get; }

        ulong Raw(int attribute)
        {
            byte* buf = stackalloc byte[Native.StringLength];
            *(ulong*)buf = 0;
            Native.Check(_context, Native.viGetAttribute(Context, (uint)attribute, buf));
            return *(ulong*)buf;
        }

        uint Context
        {
            get
            {
                if (_context == 0)
                    throw new ObjectDisposedException(nameof(NativeEventArgs));
                return _context;
            }
        }

        public byte GetAttributeByte(NativeVisaAttribute attribute) => GetAttributeByte((int)attribute);
        public byte GetAttributeByte(int attribute) => (byte)Raw(attribute);
        public short GetAttributeInt16(NativeVisaAttribute attribute) => GetAttributeInt16((int)attribute);
        public short GetAttributeInt16(int attribute) => (short)Raw(attribute);
        public int GetAttributeInt32(NativeVisaAttribute attribute) => GetAttributeInt32((int)attribute);
        public int GetAttributeInt32(int attribute) => (int)Raw(attribute);
        public long GetAttributeInt64(NativeVisaAttribute attribute) => GetAttributeInt64((int)attribute);
        public long GetAttributeInt64(int attribute) => (long)Raw(attribute);
        public bool GetAttributeBoolean(NativeVisaAttribute attribute) => GetAttributeBoolean((int)attribute);
        public bool GetAttributeBoolean(int attribute) => (ushort)Raw(attribute) != 0;
        public string GetAttributeString(NativeVisaAttribute attribute) => GetAttributeString((int)attribute);

        public string GetAttributeString(int attribute)
        {
            byte* buf = stackalloc byte[Native.StringLength];
            buf[0] = 0;
            Native.Check(_context, Native.viGetAttribute(Context, (uint)attribute, buf));
            return Native.FromZ(buf);
        }

        public void Dispose()
        {
            uint c = _context;
            _context = 0;
            if (c != 0)
                Native.viClose(c);
        }
    }
}
