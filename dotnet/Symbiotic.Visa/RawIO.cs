using System;
using System.Collections.Generic;
using System.IO;
using System.Threading;
using System.Threading.Tasks;
using Ivi.Visa;

namespace Symbiotic.Visa
{
    /// <summary>
    /// viRead and viWrite, and the asynchronous forms. The C API completes its asynchronous
    /// operations before they return, so here they are the synchronous calls on a worker thread,
    /// one after another in the order they were begun.
    /// </summary>
    internal sealed unsafe partial class RawIO : IMessageBasedRawIO
    {
        const int Chunk = 64 * 1024;

        readonly Session _session;
        readonly object _queueLock = new object();
        Task _queue = Task.FromResult(0);

        public RawIO(Session session)
        {
            _session = session;
        }

        // ------------------------------------------------------------------------------------
        // Bytes and strings. Strings are one byte per character (Latin-1): lossless for what an
        // instrument sends, which ASCII would not be.

        internal static string Latin1(byte[] b, long index, long count)
        {
            var c = new char[count];
            for (long i = 0; i < count; i++)
                c[i] = (char)b[index + i];
            return new string(c);
        }

        internal static byte[] Latin1(string s, long index, long count)
        {
            var b = new byte[count];
            for (long i = 0; i < count; i++)
            {
                char ch = s[(int)(index + i)];
                b[i] = ch <= 0xFF ? (byte)ch : (byte)'?';
            }
            return b;
        }

        static ReadStatus StatusOf(int st) =>
            st == Native.VI_SUCCESS_TERM_CHAR ? ReadStatus.TerminationCharacterEncountered
            : st == Native.VI_SUCCESS_MAX_CNT ? ReadStatus.MaximumCountReached
            : ReadStatus.EndReceived;

        /// <summary>One viRead of up to count bytes into p. A timeout is an IOTimeoutException carrying what did arrive.</summary>
        internal static long ReadInto(uint vi, byte* p, long count, out ReadStatus status)
        {
            uint n = 0;
            int st = Native.viRead(vi, p, (uint)Math.Min(count, uint.MaxValue), &n);
            if (st == Native.VI_ERROR_TMO)
            {
                var got = new byte[n];
                for (uint i = 0; i < n; i++)
                    got[i] = p[i];
                throw new IOTimeoutException(n, got, Native.Describe(vi, st));
            }
            Native.Check(vi, st);
            status = StatusOf(st);
            return n;
        }

        internal static long WriteFrom(uint vi, byte* p, long count)
        {
            long done = 0;
            while (done < count)
            {
                uint n = 0;
                int st = Native.viWrite(vi, p + done, (uint)Math.Min(count - done, uint.MaxValue), &n);
                Native.Check(vi, st);
                done += n;
                if (n == 0)
                    break;
            }
            return done;
        }

        /// <summary>Up to count bytes, however many viReads it takes to reach the end, a termination
        /// character or the count.</summary>
        byte[] ReadUpTo(long count, out ReadStatus status)
        {
            var all = new MemoryStream();
            var buf = new byte[(int)Math.Min(Chunk, Math.Max(1, count))];
            status = ReadStatus.Unknown;
            long left = count;
            while (left > 0)
            {
                long n;
                fixed (byte* p = buf)
                {
                    try
                    {
                        n = ReadInto(_session.Vi, p, Math.Min(buf.Length, left), out status);
                    }
                    catch (IOTimeoutException e)
                    {
                        all.Write(e.ActualData, 0, (int)e.ActualCount);
                        byte[] got = all.ToArray();
                        throw new IOTimeoutException(got.Length, got, e.Message);
                    }
                }
                all.Write(buf, 0, (int)n);
                left -= n;
                if (status != ReadStatus.MaximumCountReached)
                    break;
            }
            return all.ToArray();
        }

        public byte[] Read() => ReadUpTo(long.MaxValue, out _);

        public byte[] Read(long count) => ReadUpTo(count, out _);

        public byte[] Read(long count, out ReadStatus readStatus) => ReadUpTo(count, out readStatus);

        public void Read(byte[] buffer, long index, long count, out long actualCount, out ReadStatus readStatus)
        {
            CheckRange(buffer, index, count);
            fixed (byte* p = buffer)
                Read(p, index, count, out actualCount, out readStatus);
        }

        public void Read(byte* buffer, long index, long count, out long actualCount, out ReadStatus readStatus)
        {
            if (buffer == null)
                throw new ArgumentNullException(nameof(buffer));
            actualCount = ReadInto(_session.Vi, buffer + index, count, out readStatus);
        }

        public string ReadString()
        {
            byte[] b = Read();
            return Latin1(b, 0, b.Length);
        }

        public string ReadString(long count)
        {
            byte[] b = Read(count);
            return Latin1(b, 0, b.Length);
        }

        public string ReadString(long count, out ReadStatus readStatus)
        {
            byte[] b = Read(count, out readStatus);
            return Latin1(b, 0, b.Length);
        }

        public void Write(byte[] buffer) => Write(buffer, 0, buffer?.Length ?? 0);

        public void Write(byte[] buffer, long index, long count)
        {
            CheckRange(buffer, index, count);
            fixed (byte* p = buffer)
                Write(p, index, count);
        }

        public void Write(byte* buffer, long index, long count)
        {
            if (buffer == null && count > 0)
                throw new ArgumentNullException(nameof(buffer));
            WriteFrom(_session.Vi, buffer + index, count);
        }

        public void Write(string buffer) => Write(buffer, 0, buffer?.Length ?? 0);

        public void Write(string buffer, long index, long count)
        {
            if (buffer == null)
                throw new ArgumentNullException(nameof(buffer));
            if (index < 0 || count < 0 || index + count > buffer.Length)
                throw new ArgumentOutOfRangeException(nameof(count));
            Write(Latin1(buffer, index, count));
        }

        static void CheckRange(byte[] buffer, long index, long count)
        {
            if (buffer == null)
                throw new ArgumentNullException(nameof(buffer));
            if (index < 0 || count < 0 || index + count > buffer.LongLength)
                throw new ArgumentOutOfRangeException(nameof(count), $"index {index} and count {count} do not fit a buffer of {buffer.LongLength}");
        }

        // ------------------------------------------------------------------------------------
        // Asynchronous operations.

        /// <summary>One queued operation. The buffer is a managed array for its whole life, so nothing needs pinning between steps.</summary>
        internal sealed class AsyncOp : IVisaAsyncResult
        {
            // The wait handle is made only for a caller that asks for it: End and the callback do not
            // need one, and a handle per operation left to the finalizer piles up in a program that
            // runs asynchronous I/O for hours.
            readonly object _gate = new object();
            ManualResetEvent _done;
            volatile bool _completed;
            volatile bool _aborted;

            public AsyncOp(byte[] buffer, long index, long count, bool write, VisaAsyncCallback callback, object state)
            {
                Buffer = buffer;
                Index = index;
                Count = count;
                IsWrite = write;
                Callback = callback;
                AsyncState = state;
            }

            public byte[] Buffer { get; }
            public long Index { get; }
            public long Count { get; }
            public bool IsWrite { get; }
            public VisaAsyncCallback Callback { get; }
            public object AsyncState { get; }
            public WaitHandle AsyncWaitHandle
            {
                get
                {
                    lock (_gate)
                        return _done ?? (_done = new ManualResetEvent(_completed));
                }
            }
            public bool CompletedSynchronously => false;
            public bool IsCompleted => _completed;
            public bool IsAborted => _aborted;

            /// <summary>How many bytes moved; and the exception the operation ended with, if any.</summary>
            public long Actual;
            public ReadStatus ReadStatus;
            public Exception Error;

            public void Abort() => _aborted = true;

            public void Complete()
            {
                lock (_gate)
                {
                    _completed = true;
                    _done?.Set();
                    Monitor.PulseAll(_gate);
                }
                try { Callback?.Invoke(this); } catch (Exception) { }
            }

            /// <summary>Wait, then give the count or throw what ended it.</summary>
            public long End()
            {
                lock (_gate)
                    while (!_completed)
                        Monitor.Wait(_gate);
                if (_aborted)
                    throw new NativeVisaException(Native.VI_ERROR_ABORT, "the asynchronous operation was aborted");
                if (Error != null)
                    throw Error;
                return Actual;
            }
        }

        AsyncOp Begin(AsyncOp op)
        {
            uint vi = _session.Vi; // throws now if the session is closed
            lock (_queueLock)
                _queue = _queue.ContinueWith(_ => Run(vi, op), CancellationToken.None, TaskContinuationOptions.None, TaskScheduler.Default);
            return op;
        }

        void Run(uint vi, AsyncOp op)
        {
            try
            {
                if (op.IsAborted)
                    return;
                fixed (byte* p = op.Buffer)
                {
                    if (op.IsWrite)
                        op.Actual = WriteFrom(vi, p + op.Index, op.Count);
                    else
                        op.Actual = ReadInto(vi, p + op.Index, op.Count, out op.ReadStatus);
                }
            }
            catch (Exception e)
            {
                op.Error = e;
            }
            finally
            {
                op.Complete();
            }
        }

        public IVisaAsyncResult BeginWrite(string buffer) => BeginWrite(buffer, null, null);
        public IVisaAsyncResult BeginWrite(string buffer, object state) => BeginWrite(buffer, null, state);

        public IVisaAsyncResult BeginWrite(string buffer, VisaAsyncCallback callback, object state)
        {
            if (buffer == null)
                throw new ArgumentNullException(nameof(buffer));
            byte[] b = Latin1(buffer, 0, buffer.Length);
            return BeginWrite(b, 0, b.Length, callback, state);
        }

        public IVisaAsyncResult BeginWrite(byte[] buffer) => BeginWrite(buffer, 0, buffer?.Length ?? 0, null, null);
        public IVisaAsyncResult BeginWrite(byte[] buffer, object state) => BeginWrite(buffer, 0, buffer?.Length ?? 0, null, state);
        public IVisaAsyncResult BeginWrite(byte[] buffer, long index, long count) => BeginWrite(buffer, index, count, null, null);
        public IVisaAsyncResult BeginWrite(byte[] buffer, long index, long count, object state) => BeginWrite(buffer, index, count, null, state);
        public IVisaAsyncResult BeginWrite(byte[] buffer, VisaAsyncCallback callback, object state) => BeginWrite(buffer, 0, buffer?.Length ?? 0, callback, state);

        public IVisaAsyncResult BeginWrite(byte[] buffer, long index, long count, VisaAsyncCallback callback, object state)
        {
            CheckRange(buffer, index, count);
            return Begin(new AsyncOp(buffer, index, count, true, callback, state));
        }

        public long EndWrite(IVisaAsyncResult result) => Own(result).End();

        public IVisaAsyncResult BeginRead(int count) => BeginRead(new byte[count], 0, count, null, null);
        public IVisaAsyncResult BeginRead(int count, object state) => BeginRead(new byte[count], 0, count, null, state);
        public IVisaAsyncResult BeginRead(int count, VisaAsyncCallback callback, object state) => BeginRead(new byte[count], 0, count, callback, state);
        public IVisaAsyncResult BeginRead(byte[] buffer) => BeginRead(buffer, 0, buffer?.Length ?? 0, null, null);
        public IVisaAsyncResult BeginRead(byte[] buffer, object state) => BeginRead(buffer, 0, buffer?.Length ?? 0, null, state);
        public IVisaAsyncResult BeginRead(byte[] buffer, long index, long count) => BeginRead(buffer, index, count, null, null);
        public IVisaAsyncResult BeginRead(byte[] buffer, long index, long count, object state) => BeginRead(buffer, index, count, null, state);
        public IVisaAsyncResult BeginRead(byte[] buffer, VisaAsyncCallback callback, object state) => BeginRead(buffer, 0, buffer?.Length ?? 0, callback, state);

        public IVisaAsyncResult BeginRead(byte[] buffer, long index, long count, VisaAsyncCallback callback, object state)
        {
            CheckRange(buffer, index, count);
            return Begin(new AsyncOp(buffer, index, count, false, callback, state));
        }

        public long EndRead(IVisaAsyncResult result) => Own(result).End();

        public string EndReadString(IVisaAsyncResult result)
        {
            AsyncOp op = Own(result);
            long n = op.End();
            return Latin1(op.Buffer, op.Index, n);
        }

        /// <summary>One not yet started never starts; one under way runs to its end or timeout (the C
        /// API has nothing that stops it), and either way ends as aborted.</summary>
        public void AbortAsyncOperation(IVisaAsyncResult result) => Own(result).Abort();

        static AsyncOp Own(IVisaAsyncResult result)
        {
            if (result is AsyncOp op)
                return op;
            throw new ArgumentException("not an operation this session began", nameof(result));
        }

#if NET6_0_OR_GREATER
        sealed class ReadResult : IVisaReadResult
        {
            public ReadResult(long actual, ReadStatus status)
            {
                ActualCount = actual;
                ReadStatus = status;
            }

            public long ActualCount { get; }
            public ReadStatus ReadStatus { get; }
        }

        public IVisaReadResult Read(Span<byte> buffer)
        {
            fixed (byte* p = buffer)
            {
                long n = ReadInto(_session.Vi, p, buffer.Length, out ReadStatus st);
                return new ReadResult(n, st);
            }
        }

        public void Write(ReadOnlySpan<byte> buffer)
        {
            fixed (byte* p = buffer)
                WriteFrom(_session.Vi, p, buffer.Length);
        }

#endif
    }

#if NET6_0_OR_GREATER
    /// <summary>The Task forms, apart: C# allows no await in an unsafe context.</summary>
    internal sealed partial class RawIO
    {
        /// <summary>Through a private array: a Memory may not be one, and it is copied back on completion.</summary>
        public async Task<IVisaReadResult> ReadAsync(Memory<byte> buffer, CancellationToken token = default)
        {
            var tmp = new byte[buffer.Length];
            var op = await Queue(new AsyncOp(tmp, 0, tmp.Length, false, null, null), token).ConfigureAwait(false);
            tmp.AsSpan(0, (int)op.Actual).CopyTo(buffer.Span);
            return new ReadResult(op.Actual, op.ReadStatus);
        }

        public async Task<IVisaReadResult> ReadAsync(byte[] buffer, long index, long count, CancellationToken token = default)
        {
            CheckRange(buffer, index, count);
            var op = await Queue(new AsyncOp(buffer, index, count, false, null, null), token).ConfigureAwait(false);
            return new ReadResult(op.Actual, op.ReadStatus);
        }

        public async Task WriteAsync(ReadOnlyMemory<byte> buffer, CancellationToken token = default)
        {
            byte[] tmp = buffer.ToArray();
            await Queue(new AsyncOp(tmp, 0, tmp.Length, true, null, null), token).ConfigureAwait(false);
        }

        public async Task WriteAsync(byte[] buffer, long index, int count, CancellationToken token = default)
        {
            CheckRange(buffer, index, count);
            await Queue(new AsyncOp(buffer, index, count, true, null, null), token).ConfigureAwait(false);
        }

        /// <summary>The operation, queued; cancellation aborts it (see AbortAsyncOperation) and ends the task as cancelled.</summary>
        Task<AsyncOp> Queue(AsyncOp op, CancellationToken token)
        {
            token.ThrowIfCancellationRequested();
            var tcs = new TaskCompletionSource<AsyncOp>(TaskCreationOptions.RunContinuationsAsynchronously);
            var reg = token.Register(() => op.Abort());
            var wrapped = new AsyncOp(op.Buffer, op.Index, op.Count, op.IsWrite, _ =>
            {
                reg.Dispose();
                if (op.IsAborted)
                    tcs.TrySetCanceled(token);
                else if (op.Error != null)
                    tcs.TrySetException(op.Error);
                else
                    tcs.TrySetResult(op);
            }, null);
            // The callback reads the outer op's results: run the outer one, complete through the wrapper.
            uint vi = _session.Vi;
            lock (_queueLock)
                _queue = _queue.ContinueWith(_ =>
                {
                    Run(vi, op);
                    wrapped.Complete();
                }, CancellationToken.None, TaskContinuationOptions.None, TaskScheduler.Default);
            return tcs.Task;
        }
    }
#endif
}
