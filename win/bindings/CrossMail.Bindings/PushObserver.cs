// UniFFI callback interface vtable：PushObserver 跨 ABI 保活。
// vtable 结构体与委托实例必须常驻（Rust 会长期持有其指针），因此用静态字段 + AllocHGlobal。
using System;
using System.Collections.Concurrent;
using System.Runtime.InteropServices;
using System.Threading;

namespace CrossMail.Bindings;

public interface IPushObserver
{
    void OnEvent(PushEventDto evt);
}

internal static class PushObserverRegistry
{
    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate void OnEventDelegate(ulong handle, RustBuffer evt, IntPtr outReturn, ref RustCallStatus status);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate void FreeDelegate(ulong handle);

    private static readonly ConcurrentDictionary<ulong, IPushObserver> Observers = new();
    private static long _next;
    private static readonly OnEventDelegate OnEventCallback = OnEvent;
    private static readonly FreeDelegate FreeCallback = Free;
    private static readonly IntPtr VTable;

    static PushObserverRegistry()
    {
        VTable = Marshal.AllocHGlobal(IntPtr.Size * 2);
        Marshal.WriteIntPtr(VTable, 0, Marshal.GetFunctionPointerForDelegate(OnEventCallback));
        Marshal.WriteIntPtr(VTable, IntPtr.Size, Marshal.GetFunctionPointerForDelegate(FreeCallback));
        NativeMethods.uniffi_crossmail_ffi_fn_init_callback_vtable_pushobserver(VTable);
    }

    internal static ulong Register(IPushObserver observer)
    {
        _ = VTable; // 触发静态构造，确保 vtable 与委托常驻
        if (observer is null) throw new ArgumentNullException(nameof(observer));
        ulong handle = (ulong)Interlocked.Increment(ref _next);
        Observers[handle] = observer;
        return handle;
    }

    private static void OnEvent(ulong handle, RustBuffer evt, IntPtr outReturn, ref RustCallStatus status)
    {
        status.Code = 0;
        status.ErrorBuf = default;
        try
        {
            if (Observers.TryGetValue(handle, out IPushObserver? observer))
            {
                using var r = BeReader.FromBuffer(evt);
                var dto = PushEventDto.Read(r);
                r.EnsureConsumed();
                observer.OnEvent(dto);
            }
        }
        catch (Exception ex)
        {
            status.Code = 2; // CALL_UNEXPECTED_ERROR
            status.ErrorBuf = Codec.LowerString(ex.ToString());
        }
        finally
        {
            RustCall.FreeBuffer(evt);
        }
    }

    private static void Free(ulong handle) => Observers.TryRemove(handle, out _);
}
