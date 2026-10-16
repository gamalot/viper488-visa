using System;
using Ivi.Visa;

namespace Symbiotic.Visa
{
    /// <summary>VISA.NET's side of Native's failures: a timeout is an IOTimeoutException, any other error a NativeVisaException.</summary>
    internal static partial class Native
    {
        static partial void MakeLoadFailure(string message, ref Exception e) => e = new VisaException(message);

        static partial void MakeError(uint vi, int status, ref Exception e) =>
            e = status == VI_ERROR_TMO
                ? new IOTimeoutException(0, new byte[0], Describe(vi, status))
                : (Exception)new NativeVisaException(status, Describe(vi, status));
    }
}
