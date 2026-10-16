using System;
using System.Runtime.InteropServices;
using Ivi.Visa.Interop;
using Symbiotic.Visa;

namespace Symbiotic.VisaCom
{
    /// <summary>
    /// IBaseMessage, IMessage and IAsyncMessage (and IGpibIntfcMessage's same members), for every resource that
    /// talks in messages. A read's completion code is its HRESULT: VI_SUCCESS_MAX_CNT tells the caller there is
    /// more (Nimbus-built drivers read on while they get it), VI_SUCCESS_TERM_CHAR that the termination character
    /// ended it.
    /// </summary>
    [ComVisible(false)]
    public abstract unsafe class MessageBase : SessionBase
    {
        public IOProtocol IOProtocol
        {
            get => (IOProtocol)I16(Native.VI_ATTR_IO_PROT);
            set => Put(Native.VI_ATTR_IO_PROT, (ushort)value);
        }

        public bool SendEndEnabled
        {
            get => Bool(Native.VI_ATTR_SEND_END_EN);
            set => Put(Native.VI_ATTR_SEND_END_EN, value);
        }

        public byte TerminationCharacter
        {
            get => U8(Native.VI_ATTR_TERMCHAR);
            set => Put(Native.VI_ATTR_TERMCHAR, value);
        }

        public bool TerminationCharacterEnabled
        {
            get => Bool(Native.VI_ATTR_TERMCHAR_EN);
            set => Put(Native.VI_ATTR_TERMCHAR_EN, value);
        }

        public void AssertTrigger(TriggerProtocol protocol = TriggerProtocol.TRIG_PROT_DEFAULT) =>
            Check(Native.viAssertTrigger(Vi, (ushort)protocol));

        public void Clear() => Check(Native.viClear(Vi));

        public short ReadSTB()
        {
            ushort stb;
            Check(Native.viReadSTB(Vi, &stb));
            return (short)stb;
        }

        /// <summary>One viRead of up to count bytes; its completion code is the call's.</summary>
        public byte[] Read(int count)
        {
            if (count < 0)
                throw Status.Error(Status.VI_ERROR_INV_PARAMETER, "a negative count");
            var buf = new byte[count];
            uint got = 0;
            fixed (byte* p = buf)
                Check(Native.viRead(Vi, p, (uint)count, &got));
            if (got == buf.Length)
                return buf;
            var r = new byte[got];
            Array.Copy(buf, r, got);
            return r;
        }

        public string ReadString(int count)
        {
            byte[] b = Read(count);
            return Latin1.String(b, 0, b.Length);
        }

        /// <summary>The first count bytes of buffer (all of it if count is larger); the number sent.</summary>
        public int Write(ref byte[] buffer, int count)
        {
            if (buffer == null)
                throw Status.Error(Status.VI_ERROR_INV_PARAMETER, "no buffer");
            int n = Math.Min(Math.Max(0, count), buffer.Length);
            uint sent = 0;
            fixed (byte* p = buffer)
                Check(Native.viWrite(Vi, p, (uint)n, &sent));
            return (int)sent;
        }

        public int WriteString(string buffer)
        {
            byte[] b = Latin1.Bytes(buffer ?? "");
            return Write(ref b, b.Length);
        }

        // IAsyncMessage (renamed here: their signatures are IMessage's; Generated.cs maps them).

        /// <summary>viReadAsync into unmanaged memory kept until the session closes; the job id.</summary>
        public int AsyncRead(int count)
        {
            if (count < 0)
                throw Status.Error(Status.VI_ERROR_INV_PARAMETER, "a negative count");
            IntPtr buf = Marshal.AllocHGlobal(Math.Max(1, count));
            uint job = 0;
            AsyncStarting(buf);
            int st = Native.viReadAsync(Vi, (byte*)buf, (uint)count, &job);
            KeepAsyncBuffer(st < 0 ? 0 : job, buf);
            Check(st);
            return (int)job;
        }

        public int AsyncWrite(ref byte[] buffer, int count)
        {
            if (buffer == null)
                throw Status.Error(Status.VI_ERROR_INV_PARAMETER, "no buffer");
            int n = Math.Min(Math.Max(0, count), buffer.Length);
            IntPtr buf = Marshal.AllocHGlobal(Math.Max(1, n));
            Marshal.Copy(buffer, 0, buf, n);
            uint job = 0;
            AsyncStarting(buf);
            int st = Native.viWriteAsync(Vi, (byte*)buf, (uint)n, &job);
            KeepAsyncBuffer(st < 0 ? 0 : job, buf);
            Check(st);
            return (int)job;
        }

        public int AsyncWriteString(string buffer)
        {
            byte[] b = Latin1.Bytes(buffer ?? "");
            return AsyncWrite(ref b, b.Length);
        }

        public void Terminate(int jobId) => Check(Native.viTerminate(Vi, 0, (uint)jobId));

        // Members several interfaces share (IGpib, IGpibIntfc, IUsb, IHislipInstr; ISerial, ITcpipSocket;
        // ITcpipInstr2, IHislipInstr2, ITcpipSocket2).

        /// <summary>RENControlConst is numbered as viGpibControlREN's modes.</summary>
        public void ControlREN(RENControlConst mode) => Check(Native.viGpibControlREN(Vi, (ushort)mode));

        public void SetBufferSize(BufferMask mask, int size) => Check(Native.viSetBuf(Vi, (ushort)mask, (uint)Math.Max(0, size)));

        public void Flush(BufferMask mask = BufferMask.IO_IN_AND_OUT_BUFS, bool discard = false)
        {
            ushort m = 0;
            if (((int)mask & (int)BufferMask.IO_IN_BUF) != 0)
                m |= discard ? Native.VI_IO_IN_BUF_DISCARD : Native.VI_IO_IN_BUF;
            if (((int)mask & (int)BufferMask.IO_OUT_BUF) != 0)
                m |= discard ? Native.VI_IO_OUT_BUF_DISCARD : Native.VI_IO_OUT_BUF;
            Check(Native.viFlush(Vi, m));
        }

        public bool IsHislip => Bool(Native.VI_ATTR_TCPIP_IS_HISLIP);

        // HiSLIP 2.0's TLS and SASL are not done here: refused, not made up.
        static Exception NoTls(string what) => Status.Error(Native.VI_ERROR_NSUP_ATTR, $"{what}: this runtime does no TLS");
        public string ServerCertificate => throw NoTls("ServerCertificate");
        public string ServerCertificateIssuerName => throw NoTls("ServerCertificateIssuerName");
        public string ServerCertificateSubjectName => throw NoTls("ServerCertificateSubjectName");
        public string ServerCertificateExpirationDate => throw NoTls("ServerCertificateExpirationDate");
        public string TlsCipherSuite => throw NoTls("TlsCipherSuite");
        public bool ServerCertificateIsPerpetual => throw NoTls("ServerCertificateIsPerpetual");
        public string SaslMechanism => throw NoTls("SaslMechanism");

        public bool EncryptionEnabled
        {
            get => false;
            set { if (value) throw NoTls("EncryptionEnabled"); }
        }
    }

    /// <summary>GPIB INSTR.</summary>
    [ComVisible(true), ClassInterface(ClassInterfaceType.None), Guid("C3B2DE1B-88B5-40EE-AD54-1E9B143CF19C"), ProgId("Symbiotic.GpibInstr.1")]
    public sealed partial class GpibInstr : MessageBase
    {
        protected override string ClassProgId => "Symbiotic.GpibInstr.1";
        protected override bool Accepts(ushort type, string cls) => type == Native.VI_INTF_GPIB && cls == "INSTR";

        public short PrimaryAddress => I16(Native.VI_ATTR_GPIB_PRIMARY_ADDR);
        public LineState RENState => Line(Native.VI_ATTR_GPIB_REN_STATE);

        public bool RepeatAddressingEnabled
        {
            get => Bool(Native.VI_ATTR_GPIB_READDR_EN);
            set => Put(Native.VI_ATTR_GPIB_READDR_EN, value);
        }

        public short SecondaryAddress => I16(Native.VI_ATTR_GPIB_SECONDARY_ADDR);

        public bool UnaddressingEnabled
        {
            get => Bool(Native.VI_ATTR_GPIB_UNADDR_EN);
            set => Put(Native.VI_ATTR_GPIB_UNADDR_EN, value);
        }
    }

    /// <summary>GPIB INTFC: the board.</summary>
    [ComVisible(true), ClassInterface(ClassInterfaceType.None), Guid("43939028-AD5A-47D8-9FA7-B2FCAEEAEC6A"), ProgId("Symbiotic.GpibIntfc.1")]
    public sealed unsafe partial class GpibIntfc : MessageBase
    {
        protected override string ClassProgId => "Symbiotic.GpibIntfc.1";
        protected override bool Accepts(ushort type, string cls) => type == Native.VI_INTF_GPIB && cls == "INTFC";

        public GPIBAddressState AddressingState => (GPIBAddressState)I16(Native.VI_ATTR_GPIB_ADDR_STATE);
        public LineState ATNState => Line(Native.VI_ATTR_GPIB_ATN_STATE);

        public byte DevStatusByte
        {
            get => U8(Native.VI_ATTR_DEV_STATUS_BYTE);
            set => Put(Native.VI_ATTR_DEV_STATUS_BYTE, value);
        }

        public bool CICState => Bool(Native.VI_ATTR_GPIB_CIC_STATE);

        public short HS488CBLLength
        {
            get => I16(Native.VI_ATTR_GPIB_HS488_CBL_LEN);
            set => Put(Native.VI_ATTR_GPIB_HS488_CBL_LEN, (ushort)value);
        }

        public LineState NDACState => Line(Native.VI_ATTR_GPIB_NDAC_STATE);

        public short PrimaryAddress
        {
            get => I16(Native.VI_ATTR_GPIB_PRIMARY_ADDR);
            set => Put(Native.VI_ATTR_GPIB_PRIMARY_ADDR, (ushort)value);
        }

        public LineState RENState => Line(Native.VI_ATTR_GPIB_REN_STATE);

        public short SecondaryAddress
        {
            get => I16(Native.VI_ATTR_GPIB_SECONDARY_ADDR);
            set => Put(Native.VI_ATTR_GPIB_SECONDARY_ADDR, (ushort)value);
        }

        public LineState SRQState => Line(Native.VI_ATTR_GPIB_SRQ_STATE);

        public bool SysControlState
        {
            get => Bool(Native.VI_ATTR_GPIB_SYS_CNTRL_STATE);
            set => Put(Native.VI_ATTR_GPIB_SYS_CNTRL_STATE, value);
        }

        public int Command(ref byte[] buffer, int count)
        {
            if (buffer == null)
                throw Status.Error(Status.VI_ERROR_INV_PARAMETER, "no buffer");
            int n = Math.Min(Math.Max(0, count), buffer.Length);
            uint sent = 0;
            fixed (byte* p = buffer)
                Check(Native.viGpibCommand(Vi, p, (uint)n, &sent));
            return (int)sent;
        }

        /// <summary>ATNControlConst is numbered as viGpibControlATN's modes.</summary>
        public void ControlATN(ATNControlConst mode) => Check(Native.viGpibControlATN(Vi, (ushort)mode));

        public void PassControl(short primAddr, short secAddr = -1) =>
            Check(Native.viGpibPassControl(Vi, (ushort)primAddr, (ushort)secAddr));

        public void SendIFC() => Check(Native.viGpibSendIFC(Vi));
    }

    /// <summary>TCPIP INSTR: VXI-11 or HiSLIP.</summary>
    [ComVisible(true), ClassInterface(ClassInterfaceType.None), Guid("15DB83F8-EC66-43E3-88D5-6464F02236F3"), ProgId("Symbiotic.TcpipInstr.1")]
    public sealed partial class TcpipInstr : MessageBase
    {
        protected override string ClassProgId => "Symbiotic.TcpipInstr.1";
        protected override bool Accepts(ushort type, string cls) => type == Native.VI_INTF_TCPIP && cls == "INSTR";

        public string Address => Str(Native.VI_ATTR_TCPIP_ADDR);
        public string HostName => Str(Native.VI_ATTR_TCPIP_HOSTNAME);
        public string DeviceName => Str(Native.VI_ATTR_TCPIP_DEVICE_NAME);

        /// <summary>The negotiated HiSLIP version, the C API's version word (lan.rs).</summary>
        public int ProtocolVersion => I32(Native.VI_ATTR_TCPIP_HISLIP_VERSION);

        public int MaxMessage
        {
            get => I32(Native.VI_ATTR_TCPIP_HISLIP_MAX_MESSAGE_KB);
            set => Put(Native.VI_ATTR_TCPIP_HISLIP_MAX_MESSAGE_KB, (uint)value);
        }

        public bool OverlapEnabled
        {
            get => Bool(Native.VI_ATTR_TCPIP_HISLIP_OVERLAP_EN);
            set => Put(Native.VI_ATTR_TCPIP_HISLIP_OVERLAP_EN, value);
        }
    }

    /// <summary>TCPIP SOCKET.</summary>
    [ComVisible(true), ClassInterface(ClassInterfaceType.None), Guid("DD6452E4-8509-4672-A382-9FDBB606F338"), ProgId("Symbiotic.TcpipSocket.1")]
    public sealed partial class TcpipSocket : MessageBase
    {
        protected override string ClassProgId => "Symbiotic.TcpipSocket.1";
        protected override bool Accepts(ushort type, string cls) => type == Native.VI_INTF_TCPIP && cls == "SOCKET";

        public string Address => Str(Native.VI_ATTR_TCPIP_ADDR);
        public string HostName => Str(Native.VI_ATTR_TCPIP_HOSTNAME);

        public bool KeepAlive
        {
            get => Bool(Native.VI_ATTR_TCPIP_KEEPALIVE);
            set => Put(Native.VI_ATTR_TCPIP_KEEPALIVE, value);
        }

        public bool NoDelay
        {
            get => Bool(Native.VI_ATTR_TCPIP_NODELAY);
            set => Put(Native.VI_ATTR_TCPIP_NODELAY, value);
        }

        public short Port => I16(Native.VI_ATTR_TCPIP_PORT);
    }

    /// <summary>USB INSTR: a USBTMC instrument.</summary>
    [ComVisible(true), ClassInterface(ClassInterfaceType.None), Guid("A42F424D-E9B0-4E68-9737-D23A8312C165"), ProgId("Symbiotic.UsbInstr.1")]
    public sealed unsafe partial class UsbInstr : MessageBase
    {
        protected override string ClassProgId => "Symbiotic.UsbInstr.1";
        protected override bool Accepts(ushort type, string cls) => type == Native.VI_INTF_USB && cls == "INSTR";

        public short ManufacturerID => I16(Native.VI_ATTR_MANF_ID);
        public string ManufacturerName => Str(Native.VI_ATTR_MANF_NAME);
        public short ModelCode => I16(Native.VI_ATTR_MODEL_CODE);
        public string ModelName => Str(Native.VI_ATTR_MODEL_NAME);
        public bool Is4882Compliant => Bool(Native.VI_ATTR_4882_COMPLIANT);
        public string UsbSerialNumber => Str(Native.VI_ATTR_USB_SERIAL_NUM);
        public short UsbInterfaceNumber => I16(Native.VI_ATTR_USB_INTFC_NUM);
        public short UsbProtocol => I16(Native.VI_ATTR_USB_PROTOCOL);

        public short MaximumInterruptSize
        {
            get => I16(Native.VI_ATTR_USB_MAX_INTR_SIZE);
            set => Put(Native.VI_ATTR_USB_MAX_INTR_SIZE, (ushort)value);
        }

        public void ControlOut(short bmRequestType, short bRequest, short wValue, short wIndex, short wLength, ref byte[] buffer)
        {
            byte[] b = buffer ?? new byte[0];
            ushort n = (ushort)Math.Min((ushort)wLength, b.Length);
            fixed (byte* p = b)
                Check(Native.viUsbControlOut(Vi, bmRequestType, bRequest, (ushort)wValue, (ushort)wIndex, n, n == 0 ? null : p));
        }

        public byte[] ControlIn(short bmRequestType, short bRequest, short wValue, short wIndex, short wLength)
        {
            var buf = new byte[(ushort)wLength];
            ushort got = 0;
            fixed (byte* p = buf)
                Check(Native.viUsbControlIn(Vi, bmRequestType, bRequest, (ushort)wValue, (ushort)wIndex, (ushort)wLength, p, &got));
            if (got == buf.Length)
                return buf;
            var r = new byte[got];
            Array.Copy(buf, r, got);
            return r;
        }
    }

    /// <summary>ASRL INSTR: a serial port.</summary>
    [ComVisible(true), ClassInterface(ClassInterfaceType.None), Guid("3B7DB17C-669F-4930-AA55-69C5DA01DEE2"), ProgId("Symbiotic.AsrlInstr.1")]
    public sealed partial class AsrlInstr : MessageBase
    {
        protected override string ClassProgId => "Symbiotic.AsrlInstr.1";
        protected override bool Accepts(ushort type, string cls) => type == Native.VI_INTF_ASRL && cls == "INSTR";

        public int BytesAvailable => I32(Native.VI_ATTR_ASRL_AVAIL_NUM);

        public int BaudRate
        {
            get => I32(Native.VI_ATTR_ASRL_BAUD);
            set => Put(Native.VI_ATTR_ASRL_BAUD, (uint)value);
        }

        public short DataBits
        {
            get => I16(Native.VI_ATTR_ASRL_DATA_BITS);
            set => Put(Native.VI_ATTR_ASRL_DATA_BITS, (ushort)value);
        }

        public LineState ClearToSendState => Line(Native.VI_ATTR_ASRL_CTS_STATE);
        public LineState DataCarrierDetectState => Line(Native.VI_ATTR_ASRL_DCD_STATE);
        public LineState DataSetReadyState => Line(Native.VI_ATTR_ASRL_DSR_STATE);

        public LineState DataTerminalReadyState
        {
            get => Line(Native.VI_ATTR_ASRL_DTR_STATE);
            set => Put(Native.VI_ATTR_ASRL_DTR_STATE, (ushort)(short)value);
        }

        /// <summary>The VISA COM enumerations below are numbered as VI_ASRL_*.</summary>
        public SerialEndConst EndIn
        {
            get => (SerialEndConst)I16(Native.VI_ATTR_ASRL_END_IN);
            set => Put(Native.VI_ATTR_ASRL_END_IN, (ushort)value);
        }

        public SerialEndConst EndOut
        {
            get => (SerialEndConst)I16(Native.VI_ATTR_ASRL_END_OUT);
            set => Put(Native.VI_ATTR_ASRL_END_OUT, (ushort)value);
        }

        public SerialFlowControl FlowControl
        {
            get => (SerialFlowControl)I16(Native.VI_ATTR_ASRL_FLOW_CNTRL);
            set => Put(Native.VI_ATTR_ASRL_FLOW_CNTRL, (ushort)value);
        }

        public SerialParity Parity
        {
            get => (SerialParity)I16(Native.VI_ATTR_ASRL_PARITY);
            set => Put(Native.VI_ATTR_ASRL_PARITY, (ushort)value);
        }

        public LineState RingIndicatorState => Line(Native.VI_ATTR_ASRL_RI_STATE);

        public LineState RequestToSendState
        {
            get => Line(Native.VI_ATTR_ASRL_RTS_STATE);
            set => Put(Native.VI_ATTR_ASRL_RTS_STATE, (ushort)(short)value);
        }

        public SerialStopBits StopBits
        {
            get => (SerialStopBits)I16(Native.VI_ATTR_ASRL_STOP_BITS);
            set => Put(Native.VI_ATTR_ASRL_STOP_BITS, (ushort)value);
        }

        public byte ReplacementCharacter
        {
            get => U8(Native.VI_ATTR_ASRL_REPLACE_CHAR);
            set => Put(Native.VI_ATTR_ASRL_REPLACE_CHAR, value);
        }

        public byte XONCharacter
        {
            get => U8(Native.VI_ATTR_ASRL_XON_CHAR);
            set => Put(Native.VI_ATTR_ASRL_XON_CHAR, value);
        }

        public byte XOFFCharacter
        {
            get => U8(Native.VI_ATTR_ASRL_XOFF_CHAR);
            set => Put(Native.VI_ATTR_ASRL_XOFF_CHAR, value);
        }
    }
}
