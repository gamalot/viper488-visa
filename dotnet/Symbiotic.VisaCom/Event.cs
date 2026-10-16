using System;
using System.Runtime.InteropServices;
using Ivi.Visa.Interop;
using Symbiotic.Visa;

namespace Symbiotic.VisaCom
{
    /// <summary>
    /// A VISA COM event. From WaitOnEvent it holds its C context open, for GetAttribute, until Close; from a
    /// handler it is read in full at once, since the C side closes the context when the handler returns. An I/O
    /// completion also answers IEventIOCompletion, with the bytes an asynchronous read got.
    /// </summary>
    [ComVisible(true), ClassInterface(ClassInterfaceType.None), Guid("DEC0ABF6-A18C-4F79-81E9-C3F790AF6033")]
    public sealed unsafe class Event : ComObject, IEventIOCompletion, IEvent
    {
        uint _context;
        readonly uint _type;
        readonly int _ioStatus, _jobId, _returnCount;
        readonly byte[] _read;

        Event(uint type, uint context, bool keep, SessionBase session)
        {
            _type = type;
            if (type == Native.VI_EVENT_IO_COMPLETION)
            {
                _ioStatus = (int)Get(context, Native.VI_ATTR_STATUS);
                _jobId = (int)Get(context, Native.VI_ATTR_JOB_ID);
                _returnCount = (int)Get(context, Native.VI_ATTR_RET_COUNT_32);
                string oper = GetString(context, Native.VI_ATTR_OPER_NAME);
                // From the session's own copy of the buffer, by job: VI_ATTR_BUFFER's address may have been let
                // go already (SessionBase, on asynchronous reads).
                if (oper == "viReadAsync")
                    _read = session != null
                        ? session.ReadAsyncBuffer((uint)_jobId, (IntPtr)(long)Get(context, Native.VI_ATTR_BUFFER), _returnCount)
                        : new byte[0];
            }
            if (keep)
                _context = context;
        }

        internal static Event Queued(SessionBase s, uint type, uint context) => new Event(type, context, true, s);

        internal static Event Handled(SessionBase s, uint type, uint context) => new Event(type, context, false, s);

        static ulong Get(uint context, uint attribute)
        {
            ulong v = 0;
            byte* buf = stackalloc byte[Native.StringLength];
            *(ulong*)buf = 0;
            if (Native.viGetAttribute(context, attribute, buf) >= 0)
                v = *(ulong*)buf;
            return v;
        }

        static string GetString(uint context, uint attribute)
        {
            byte* buf = stackalloc byte[Native.StringLength];
            buf[0] = 0;
            return Native.viGetAttribute(context, attribute, buf) >= 0 ? Native.FromZ(buf) : "";
        }

        public EventType type => Enum.IsDefined(typeof(EventType), unchecked((int)_type)) ? (EventType)unchecked((int)_type) : EventType.CUSTOM_EVENT_TYPE;

        public int CustomEventTypeNumber => unchecked((int)_type);

        public object GetAttribute(int attribute)
        {
            if (_context == 0)
                throw Status.Error(Status.VI_ERROR_INV_SESSION, "the event's context is gone (a handler's event, or closed)");
            uint a = (uint)attribute;
            if (!AttributeTypes.Kinds.TryGetValue(a, out AttrKind kind))
                kind = AttrKind.U32;
            byte* buf = stackalloc byte[Native.StringLength];
            *(ulong*)buf = 0;
            Check(_context, Native.viGetAttribute(_context, a, buf));
            ulong v = *(ulong*)buf;
            switch (kind)
            {
                case AttrKind.Str: return Native.FromZ(buf);
                case AttrKind.Bool: return (ushort)v != 0;
                case AttrKind.U8: return (byte)v;
                case AttrKind.I16: case AttrKind.U16: return (short)v;
                case AttrKind.U64: case AttrKind.Ptr: return (long)v;
                default: return (int)v;
            }
        }

        public void SetAttribute(int attribute, object attrState) =>
            throw Status.Error(Status.VI_ERROR_ATTR_READONLY, "an event's attributes are read-only");

        public void Close()
        {
            uint c = _context;
            _context = 0;
            if (c != 0)
                Native.viClose(c);
        }

        ~Event()
        {
            if (_context != 0)
                Native.viClose(_context);
        }

        public int IOStatus => _ioStatus;
        public int jobId => _jobId;
        public int ReturnCount => _returnCount;
        public byte[] ReadBuffer => _read ?? new byte[0];
        public string ReadBufferAsString => _read == null ? "" : Latin1.String(_read, 0, _read.Length);
    }

    internal static class Latin1
    {
        /// <summary>One character per byte: lossless for what an instrument sends, which ASCII would not be.</summary>
        public static string String(byte[] b, int index, int count)
        {
            var c = new char[count];
            for (int i = 0; i < count; i++)
                c[i] = (char)b[index + i];
            return new string(c);
        }

        public static byte[] Bytes(string s)
        {
            var b = new byte[s.Length];
            for (int i = 0; i < s.Length; i++)
                b[i] = s[i] <= 0xFF ? (byte)s[i] : (byte)'?';
            return b;
        }
    }
}
