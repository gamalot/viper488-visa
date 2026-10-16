using System;
using Ivi.Visa;
using Ivi.Visa.FormattedIO;

namespace Symbiotic.Visa
{
    /// <summary>What an instrument session that talks in messages has: VPP-4.3's INSTR (and SOCKET).</summary>
    public abstract unsafe class MessageSession : Session, IMessageBasedSession
    {
        readonly RawIO _raw;
        IMessageBasedFormattedIO _formatted;
        readonly NativeEvent<VisaEventArgs> _srq;

        internal MessageSession(uint rm, uint vi) : base(rm, vi)
        {
            _raw = new RawIO(this);
            _srq = new NativeEvent<VisaEventArgs>(this, Native.VI_EVENT_SERVICE_REQ);
        }

        public IOProtocol IOProtocol
        {
            get => ProtocolOf(GetU16(Native.VI_ATTR_IO_PROT));
            set => Set(Native.VI_ATTR_IO_PROT, ProtocolWord(value));
        }

        /// <summary>VI_PROT_NORMAL is 1, IOProtocol.Normal 0: the enumerations are one apart.</summary>
        internal static IOProtocol ProtocolOf(ushort p) => p >= 1 && p <= 5 ? (IOProtocol)(p - 1) : IOProtocol.Normal;
        internal static ushort ProtocolWord(IOProtocol p) => (ushort)((int)p + 1);

        public bool SendEndEnabled
        {
            get => GetBool(Native.VI_ATTR_SEND_END_EN);
            set => SetBool(Native.VI_ATTR_SEND_END_EN, value);
        }

        public byte TerminationCharacter
        {
            get => GetU8(Native.VI_ATTR_TERMCHAR);
            set => Set(Native.VI_ATTR_TERMCHAR, value);
        }

        public bool TerminationCharacterEnabled
        {
            get => GetBool(Native.VI_ATTR_TERMCHAR_EN);
            set => SetBool(Native.VI_ATTR_TERMCHAR_EN, value);
        }

        /// <summary>The IVI shared components' own formatted I/O, over this session's RawIO.</summary>
        public IMessageBasedFormattedIO FormattedIO => _formatted ?? (_formatted = new MessageBasedFormattedIO(this));

        public IMessageBasedRawIO RawIO => _raw;

        public event EventHandler<VisaEventArgs> ServiceRequest
        {
            add => _srq.Add(value);
            remove => _srq.Remove(value);
        }

        public void AssertTrigger() => Check(Native.viAssertTrigger(Vi, Native.VI_TRIG_PROT_DEFAULT));

        public void Clear()
        {
            Check(Native.viClear(Vi));
            (_formatted as MessageBasedFormattedIO)?.DiscardBuffers();
        }

        public StatusByteFlags ReadStatusByte()
        {
            ushort stb;
            Check(Native.viReadSTB(Vi, &stb));
            return (StatusByteFlags)(stb & 0xFF);
        }

        /// <summary>VPP-4.3.6's RemoteLocalMode, as viGpibControlREN's modes.</summary>
        internal static ushort RenMode(RemoteLocalMode mode)
        {
            switch (mode)
            {
                case RemoteLocalMode.LocalWithoutLockout: return 2; // VI_GPIB_REN_DEASSERT_GTL
                case RemoteLocalMode.Remote: return 3;              // VI_GPIB_REN_ASSERT_ADDRESS
                case RemoteLocalMode.RemoteWithLocalLockout: return 5; // VI_GPIB_REN_ASSERT_ADDRESS_LLO
                case RemoteLocalMode.Local: return 6;               // VI_GPIB_REN_ADDRESS_GTL
                default: throw new ArgumentOutOfRangeException(nameof(mode));
            }
        }

        public void SendRemoteLocalCommand(RemoteLocalMode mode) => Check(Native.viGpibControlREN(Vi, RenMode(mode)));

        /// <summary>viSetBuf; false when the C API has no such buffer to size (a warning, not an error).</summary>
        public bool SetBufferSize(IOBuffers buffers, int size)
        {
            ushort mask = 0;
            if ((buffers & IOBuffers.Read) != 0)
                mask |= Native.VI_IO_IN_BUF;
            if ((buffers & IOBuffers.Write) != 0)
                mask |= Native.VI_IO_OUT_BUF;
            return Check(Native.viSetBuf(Vi, mask, (uint)Math.Max(0, size))) == Native.VI_SUCCESS;
        }

        public void Flush(IOBuffers buffers, bool discard)
        {
            ushort mask = 0;
            if ((buffers & IOBuffers.Read) != 0)
                mask |= discard ? Native.VI_IO_IN_BUF_DISCARD : Native.VI_IO_IN_BUF;
            if ((buffers & IOBuffers.Write) != 0)
                mask |= discard ? Native.VI_IO_OUT_BUF_DISCARD : Native.VI_IO_OUT_BUF;
            Check(Native.viFlush(Vi, mask));
        }

        internal static Exception NotSupported(string what) =>
            new NativeVisaException(Native.VI_ERROR_NSUP_ATTR, $"{what}: not supported by this runtime");
    }

    /// <summary>GPIB INSTR.</summary>
    public sealed unsafe class GpibSession : MessageSession, IGpibSession
    {
        internal GpibSession(uint rm, uint vi) : base(rm, vi) { }

        public bool AllowDma
        {
            get => GetBool(Native.VI_ATTR_DMA_ALLOW_EN);
            set => SetBool(Native.VI_ATTR_DMA_ALLOW_EN, value);
        }

        public short PrimaryAddress => GetI16(Native.VI_ATTR_GPIB_PRIMARY_ADDR);

        public bool ReaddressingEnabled
        {
            get => GetBool(Native.VI_ATTR_GPIB_READDR_EN);
            set => SetBool(Native.VI_ATTR_GPIB_READDR_EN, value);
        }

        public LineState RenState => GetLine(Native.VI_ATTR_GPIB_REN_STATE);

        /// <summary>VI_NO_SEC_ADDR (0xFFFF) is VisaConstants.GpibNoSecondaryAddress (-1): the same 16 bits.</summary>
        public short SecondaryAddress => GetI16(Native.VI_ATTR_GPIB_SECONDARY_ADDR);

        public bool UnaddressingEnabled
        {
            get => GetBool(Native.VI_ATTR_GPIB_UNADDR_EN);
            set => SetBool(Native.VI_ATTR_GPIB_UNADDR_EN, value);
        }

        /// <summary>VPP-4.3.6 lists these in viGpibControlREN's order, skipping VI_GPIB_REN_ASSERT_LLO (an interface's).</summary>
        public void SendRemoteLocalCommand(GpibInstrumentRemoteLocalMode mode)
        {
            ushort m;
            switch (mode)
            {
                case GpibInstrumentRemoteLocalMode.DeassertRen: m = 0; break;
                case GpibInstrumentRemoteLocalMode.AssertRen: m = 1; break;
                case GpibInstrumentRemoteLocalMode.GoToLocalDeassertRen: m = 2; break;
                case GpibInstrumentRemoteLocalMode.AddressDeviceAssertRen: m = 3; break;
                case GpibInstrumentRemoteLocalMode.AddressDeviceSendLocalLockout: m = 5; break;
                case GpibInstrumentRemoteLocalMode.GoToLocal: m = 6; break;
                default: throw new ArgumentOutOfRangeException(nameof(mode));
            }
            Check(Native.viGpibControlREN(Vi, m));
        }
    }

    /// <summary>GPIB INTFC: the board itself.</summary>
    public sealed unsafe class GpibInterfaceSession : Session, IGpibInterfaceSession
    {
        readonly RawIO _raw;
        readonly NativeEvent<VisaEventArgs> _srq, _cleared, _listen, _talk, _trigger;
        readonly NativeEvent<GpibControllerInChargeEventArgs> _cic;

        internal GpibInterfaceSession(uint rm, uint vi) : base(rm, vi)
        {
            _raw = new RawIO(this);
            _srq = new NativeEvent<VisaEventArgs>(this, Native.VI_EVENT_SERVICE_REQ);
            _cleared = new NativeEvent<VisaEventArgs>(this, Native.VI_EVENT_CLEAR);
            _listen = new NativeEvent<VisaEventArgs>(this, Native.VI_EVENT_GPIB_LISTEN);
            _talk = new NativeEvent<VisaEventArgs>(this, Native.VI_EVENT_GPIB_TALK);
            _trigger = new NativeEvent<VisaEventArgs>(this, Native.VI_EVENT_TRIG);
            _cic = new NativeEvent<GpibControllerInChargeEventArgs>(this, Native.VI_EVENT_GPIB_CIC);
        }

        public GpibAddressedState AddressState
        {
            get
            {
                short s = GetI16(Native.VI_ATTR_GPIB_ADDR_STATE);
                return s == 1 ? GpibAddressedState.Talker : s == 2 ? GpibAddressedState.Listener : GpibAddressedState.Unaddressed;
            }
        }

        public bool AllowDma
        {
            get => GetBool(Native.VI_ATTR_DMA_ALLOW_EN);
            set => SetBool(Native.VI_ATTR_DMA_ALLOW_EN, value);
        }

        public LineState AtnState => GetLine(Native.VI_ATTR_GPIB_ATN_STATE);

        public short HS488CableLength
        {
            get => GetI16(Native.VI_ATTR_GPIB_HS488_CBL_LEN);
            set => Set(Native.VI_ATTR_GPIB_HS488_CBL_LEN, (ushort)value);
        }

        public byte DeviceStatusByte
        {
            get => GetU8(Native.VI_ATTR_DEV_STATUS_BYTE);
            set => Set(Native.VI_ATTR_DEV_STATUS_BYTE, value);
        }

        public IOProtocol IOProtocol
        {
            get => MessageSession.ProtocolOf(GetU16(Native.VI_ATTR_IO_PROT));
            set => Set(Native.VI_ATTR_IO_PROT, MessageSession.ProtocolWord(value));
        }

        public bool IsControllerInCharge => GetBool(Native.VI_ATTR_GPIB_CIC_STATE);

        public bool IsSystemController
        {
            get => GetBool(Native.VI_ATTR_GPIB_SYS_CNTRL_STATE);
            set => SetBool(Native.VI_ATTR_GPIB_SYS_CNTRL_STATE, value);
        }

        public LineState NdacState => GetLine(Native.VI_ATTR_GPIB_NDAC_STATE);

        public short PrimaryAddress
        {
            get => GetI16(Native.VI_ATTR_GPIB_PRIMARY_ADDR);
            set => Set(Native.VI_ATTR_GPIB_PRIMARY_ADDR, (ushort)value);
        }

        public LineState RenState => GetLine(Native.VI_ATTR_GPIB_REN_STATE);

        public short SecondaryAddress
        {
            get => GetI16(Native.VI_ATTR_GPIB_SECONDARY_ADDR);
            set => Set(Native.VI_ATTR_GPIB_SECONDARY_ADDR, (ushort)value);
        }

        public bool SendEndEnabled
        {
            get => GetBool(Native.VI_ATTR_SEND_END_EN);
            set => SetBool(Native.VI_ATTR_SEND_END_EN, value);
        }

        public LineState SrqState => GetLine(Native.VI_ATTR_GPIB_SRQ_STATE);

        public byte TerminationCharacter
        {
            get => GetU8(Native.VI_ATTR_TERMCHAR);
            set => Set(Native.VI_ATTR_TERMCHAR, value);
        }

        public bool TerminationCharacterEnabled
        {
            get => GetBool(Native.VI_ATTR_TERMCHAR_EN);
            set => SetBool(Native.VI_ATTR_TERMCHAR_EN, value);
        }

        public IMessageBasedRawIO RawIO => _raw;

        public event EventHandler<VisaEventArgs> Cleared { add => _cleared.Add(value); remove => _cleared.Remove(value); }
        public event EventHandler<GpibControllerInChargeEventArgs> ControllerInCharge { add => _cic.Add(value); remove => _cic.Remove(value); }
        public event EventHandler<VisaEventArgs> Listen { add => _listen.Add(value); remove => _listen.Remove(value); }
        public event EventHandler<VisaEventArgs> ServiceRequest { add => _srq.Add(value); remove => _srq.Remove(value); }
        public event EventHandler<VisaEventArgs> Talk { add => _talk.Add(value); remove => _talk.Remove(value); }
        public event EventHandler<VisaEventArgs> Trigger { add => _trigger.Add(value); remove => _trigger.Remove(value); }

        public void AssertTrigger() => Check(Native.viAssertTrigger(Vi, Native.VI_TRIG_PROT_DEFAULT));

        public void PassControl(short primaryAddress) => PassControl(primaryAddress, VisaConstants.GpibNoSecondaryAddress);

        public void PassControl(short primaryAddress, short secondaryAddress) =>
            Check(Native.viGpibPassControl(Vi, (ushort)primaryAddress, (ushort)secondaryAddress));

        /// <summary>AtnMode is numbered as viGpibControlATN's modes.</summary>
        public void ControlAtn(AtnMode command) => Check(Native.viGpibControlATN(Vi, (ushort)command));

        public int SendCommand(byte[] data)
        {
            if (data == null)
                throw new ArgumentNullException(nameof(data));
            uint n = 0;
            fixed (byte* p = data)
                Check(Native.viGpibCommand(Vi, p, (uint)data.Length, &n));
            return (int)n;
        }

        public void SendRemoteLocalCommand(GpibInterfaceRemoteLocalMode mode)
        {
            ushort m;
            switch (mode)
            {
                case GpibInterfaceRemoteLocalMode.DeassertRen: m = 0; break;
                case GpibInterfaceRemoteLocalMode.AssertRen: m = 1; break;
                case GpibInterfaceRemoteLocalMode.LocalLockoutAssertRen: m = 4; break; // VI_GPIB_REN_ASSERT_LLO
                default: throw new ArgumentOutOfRangeException(nameof(mode));
            }
            Check(Native.viGpibControlREN(Vi, m));
        }

        public void SendInterfaceClear() => Check(Native.viGpibSendIFC(Vi));
    }

    /// <summary>TCPIP INSTR: VXI-11 or HiSLIP.</summary>
    public sealed unsafe class TcpipSession : MessageSession, ITcpipSession
#if !NET6_0_OR_GREATER
        , ITcpipSession2
#endif
    {
        internal TcpipSession(uint rm, uint vi) : base(rm, vi) { }

        public string Address => GetString(Native.VI_ATTR_TCPIP_ADDR);
        public string DeviceName => GetString(Native.VI_ATTR_TCPIP_DEVICE_NAME);
        public string HostName => GetString(Native.VI_ATTR_TCPIP_HOSTNAME);
        public short Port => GetI16(Native.VI_ATTR_TCPIP_PORT);
        public bool IsHiSLIP => GetBool(Native.VI_ATTR_TCPIP_IS_HISLIP);

        public bool HiSLIPOverlapEnabled
        {
            get => GetBool(Native.VI_ATTR_TCPIP_HISLIP_OVERLAP_EN);
            set => SetBool(Native.VI_ATTR_TCPIP_HISLIP_OVERLAP_EN, value);
        }

        /// <summary>The negotiated version, as the C API gives it: a VPP-4.3 version word (see lan.rs).</summary>
        public Version HiSLIPProtocolVersion => FromVersionWord(GetU32(Native.VI_ATTR_TCPIP_HISLIP_VERSION));

        public int HiSLIPMaximumMessageKBytes
        {
            get => GetI32(Native.VI_ATTR_TCPIP_HISLIP_MAX_MESSAGE_KB);
            set => Set(Native.VI_ATTR_TCPIP_HISLIP_MAX_MESSAGE_KB, (uint)value);
        }

        // HiSLIP 2.0's encryption is not done here: never on, and asking for it or for its certificate is refused
        // rather than answered with something made up.

        public bool EncryptionEnabled
        {
            get => false;
            set { if (value) throw NotSupported("HiSLIP encryption"); }
        }

        public string SaslMechanism => throw NotSupported("SASL");
        public string ServerCertificate => throw NotSupported("TLS");
        public DateTime ServerCertificateExpirationDate => throw NotSupported("TLS");
        public bool ServerCertificateIsPerpetual => throw NotSupported("TLS");
        public string ServerCertificateIssuerName => throw NotSupported("TLS");
        public string ServerCertificateSubjectName => throw NotSupported("TLS");
        public string TlsCipherSuite => throw NotSupported("TLS");
    }

    /// <summary>TCPIP SOCKET: a raw socket.</summary>
    public sealed unsafe class TcpipSocketSession : MessageSession, ITcpipSocketSession
#if !NET6_0_OR_GREATER
        , ITcpipSocketSession2
#endif
    {
        internal TcpipSocketSession(uint rm, uint vi) : base(rm, vi) { }

        public string Address => GetString(Native.VI_ATTR_TCPIP_ADDR);
        public string HostName => GetString(Native.VI_ATTR_TCPIP_HOSTNAME);
        public short Port => GetI16(Native.VI_ATTR_TCPIP_PORT);

        public bool KeepAlive
        {
            get => GetBool(Native.VI_ATTR_TCPIP_KEEPALIVE);
            set => SetBool(Native.VI_ATTR_TCPIP_KEEPALIVE, value);
        }

        public bool NoDelay
        {
            get => GetBool(Native.VI_ATTR_TCPIP_NODELAY);
            set => SetBool(Native.VI_ATTR_TCPIP_NODELAY, value);
        }

        public string ServerCertificate => throw NotSupported("TLS");
        public DateTime ServerCertificateExpirationDate => throw NotSupported("TLS");
        public bool ServerCertificateIsPerpetual => throw NotSupported("TLS");
        public string ServerCertificateIssuerName => throw NotSupported("TLS");
        public string ServerCertificateSubjectName => throw NotSupported("TLS");
        public string TlsCipherSuite => throw NotSupported("TLS");
    }

    /// <summary>USB INSTR: a USBTMC instrument.</summary>
    public sealed unsafe class UsbSession : MessageSession, IUsbSession
    {
        readonly NativeEvent<UsbInterruptEventArgs> _interrupt;

        internal UsbSession(uint rm, uint vi) : base(rm, vi)
        {
            _interrupt = new NativeEvent<UsbInterruptEventArgs>(this, Native.VI_EVENT_USB_INTR);
        }

        public bool Is4882Compliant => GetBool(Native.VI_ATTR_4882_COMPLIANT);

        public short MaximumInterruptSize
        {
            get => GetI16(Native.VI_ATTR_USB_MAX_INTR_SIZE);
            set => Set(Native.VI_ATTR_USB_MAX_INTR_SIZE, (ushort)value);
        }

        public short ManufacturerId => GetI16(Native.VI_ATTR_MANF_ID);
        public string ManufacturerName => GetString(Native.VI_ATTR_MANF_NAME);
        public short ModelCode => GetI16(Native.VI_ATTR_MODEL_CODE);
        public string ModelName => GetString(Native.VI_ATTR_MODEL_NAME);
        public short UsbInterfaceNumber => GetI16(Native.VI_ATTR_USB_INTFC_NUM);
        public short UsbProtocol => GetI16(Native.VI_ATTR_USB_PROTOCOL);
        public string UsbSerialNumber => GetString(Native.VI_ATTR_USB_SERIAL_NUM);

        public event EventHandler<UsbInterruptEventArgs> Interrupt
        {
            add => _interrupt.Add(value);
            remove => _interrupt.Remove(value);
        }

        public byte[] ControlIn(short requestType, short request, short value, short index, short length)
        {
            var buf = new byte[(ushort)length];
            ushort got = 0;
            fixed (byte* p = buf)
                Check(Native.viUsbControlIn(Vi, requestType, request, (ushort)value, (ushort)index, (ushort)length, p, &got));
            if (got == buf.Length)
                return buf;
            var r = new byte[got];
            Array.Copy(buf, r, got);
            return r;
        }

        public void ControlOut(short requestType, short request, short value, short index) =>
            Check(Native.viUsbControlOut(Vi, requestType, request, (ushort)value, (ushort)index, 0, null));

        public void ControlOut(short requestType, short request, short value, short index, byte[] data)
        {
            data = data ?? new byte[0];
            fixed (byte* p = data)
                Check(Native.viUsbControlOut(Vi, requestType, request, (ushort)value, (ushort)index, (ushort)data.Length, p));
        }
    }

    /// <summary>ASRL INSTR: a serial port.</summary>
    public sealed unsafe class SerialSession : MessageSession, ISerialSession
    {
        internal SerialSession(uint rm, uint vi) : base(rm, vi) { }

        public int BytesAvailable => GetI32(Native.VI_ATTR_ASRL_AVAIL_NUM);

        public int BaudRate
        {
            get => GetI32(Native.VI_ATTR_ASRL_BAUD);
            set => Set(Native.VI_ATTR_ASRL_BAUD, (uint)value);
        }

        public LineState ClearToSendState => GetLine(Native.VI_ATTR_ASRL_CTS_STATE);

        public short DataBits
        {
            get => GetI16(Native.VI_ATTR_ASRL_DATA_BITS);
            set => Set(Native.VI_ATTR_ASRL_DATA_BITS, (ushort)value);
        }

        public LineState DataCarrierDetectState => GetLine(Native.VI_ATTR_ASRL_DCD_STATE);
        public LineState DataSetReadyState => GetLine(Native.VI_ATTR_ASRL_DSR_STATE);

        public LineState DataTerminalReadyState
        {
            get => GetLine(Native.VI_ATTR_ASRL_DTR_STATE);
            set => Set(Native.VI_ATTR_ASRL_DTR_STATE, LineWord(value));
        }

        /// <summary>SerialFlowControlModes has VI_ASRL_FLOW_*'s bits.</summary>
        public SerialFlowControlModes FlowControl
        {
            get => (SerialFlowControlModes)GetU16(Native.VI_ATTR_ASRL_FLOW_CNTRL);
            set => Set(Native.VI_ATTR_ASRL_FLOW_CNTRL, (ushort)value);
        }

        /// <summary>SerialParity is numbered as VI_ASRL_PAR_*.</summary>
        public SerialParity Parity
        {
            get => (SerialParity)GetU16(Native.VI_ATTR_ASRL_PARITY);
            set => Set(Native.VI_ATTR_ASRL_PARITY, (ushort)value);
        }

        /// <summary>SerialTerminationMethod is numbered as VI_ASRL_END_*.</summary>
        public SerialTerminationMethod ReadTermination
        {
            get => (SerialTerminationMethod)GetU16(Native.VI_ATTR_ASRL_END_IN);
            set => Set(Native.VI_ATTR_ASRL_END_IN, (ushort)value);
        }

        public byte ReplacementCharacter
        {
            get => GetU8(Native.VI_ATTR_ASRL_REPLACE_CHAR);
            set => Set(Native.VI_ATTR_ASRL_REPLACE_CHAR, value);
        }

        public LineState RequestToSendState
        {
            get => GetLine(Native.VI_ATTR_ASRL_RTS_STATE);
            set => Set(Native.VI_ATTR_ASRL_RTS_STATE, LineWord(value));
        }

        public LineState RingIndicatorState => GetLine(Native.VI_ATTR_ASRL_RI_STATE);

        /// <summary>VI_ASRL_STOP_ONE is 10, ONE5 15, TWO 20.</summary>
        public SerialStopBitsMode StopBits
        {
            get
            {
                ushort s = GetU16(Native.VI_ATTR_ASRL_STOP_BITS);
                return s == 15 ? SerialStopBitsMode.OneAndOneHalf : s == 20 ? SerialStopBitsMode.Two : SerialStopBitsMode.One;
            }
            set => Set(Native.VI_ATTR_ASRL_STOP_BITS, value == SerialStopBitsMode.OneAndOneHalf ? 15u : value == SerialStopBitsMode.Two ? 20u : 10u);
        }

        public SerialTerminationMethod WriteTermination
        {
            get => (SerialTerminationMethod)GetU16(Native.VI_ATTR_ASRL_END_OUT);
            set => Set(Native.VI_ATTR_ASRL_END_OUT, (ushort)value);
        }

        public byte XOffCharacter
        {
            get => GetU8(Native.VI_ATTR_ASRL_XOFF_CHAR);
            set => Set(Native.VI_ATTR_ASRL_XOFF_CHAR, value);
        }

        public byte XOnCharacter
        {
            get => GetU8(Native.VI_ATTR_ASRL_XON_CHAR);
            set => Set(Native.VI_ATTR_ASRL_XON_CHAR, value);
        }

        static ulong LineWord(LineState s) =>
            s == LineState.Asserted ? 1UL : s == LineState.Unasserted ? 0UL
            : throw new ArgumentOutOfRangeException(nameof(s), "a line can be set asserted or unasserted, not unknown");
    }
}
