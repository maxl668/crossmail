// CrossMail Windows 侧 C ABI 运行时：UniFFI 0.28 大端序列化 + RustCallStatus + 句柄生命周期。
// 对照 build/bindings/python/crossmail_ffi.py 的 ctypes 定义实现，字段布局逐项对齐。
using System;
using System.Buffers.Binary;
using System.Collections.Generic;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;

namespace CrossMail.Bindings;

[StructLayout(LayoutKind.Sequential)]
internal struct RustBuffer
{
    public ulong Capacity;
    public ulong Len;
    public IntPtr Data;
    public bool IsEmpty => Len == 0 || Data == IntPtr.Zero;
}

[StructLayout(LayoutKind.Sequential)]
internal struct ForeignBytes
{
    public int Len;
    public IntPtr Data;
}

[StructLayout(LayoutKind.Sequential)]
internal struct RustCallStatus
{
    public sbyte Code;
    public RustBuffer ErrorBuf;
}

internal sealed class CallStatusBox
{
    public RustCallStatus Status;
}

public sealed class CrossMailBindingException : Exception
{
    public CrossMailBindingException(string message) : base(message) { }
}

internal sealed class BeWriter : IDisposable
{
    private readonly MemoryStream _ms = new();

    public void WriteU8(byte v) => _ms.WriteByte(v);

    public void WriteI32(int v)
    {
        Span<byte> b = stackalloc byte[4];
        BinaryPrimitives.WriteInt32BigEndian(b, v);
        _ms.Write(b);
    }

    public void WriteI64(long v)
    {
        Span<byte> b = stackalloc byte[8];
        BinaryPrimitives.WriteInt64BigEndian(b, v);
        _ms.Write(b);
    }

    public void WriteU16(ushort v)
    {
        Span<byte> b = stackalloc byte[2];
        BinaryPrimitives.WriteUInt16BigEndian(b, v);
        _ms.Write(b);
    }

    public void WriteU32(uint v)
    {
        Span<byte> b = stackalloc byte[4];
        BinaryPrimitives.WriteUInt32BigEndian(b, v);
        _ms.Write(b);
    }

    public void WriteU64(ulong v)
    {
        Span<byte> b = stackalloc byte[8];
        BinaryPrimitives.WriteUInt64BigEndian(b, v);
        _ms.Write(b);
    }

    public void WriteRaw(byte[] bytes) => _ms.Write(bytes, 0, bytes.Length);

    public byte[] ToArray() => _ms.ToArray();

    public void Dispose() => _ms.Dispose();
}

internal sealed class BeReader : IDisposable
{
    private readonly byte[] _data;
    private int _pos;

    private BeReader(byte[] data) { _data = data; }

    public static BeReader FromBuffer(RustBuffer buf)
    {
        int len = checked((int)buf.Len);
        var data = new byte[len];
        if (len > 0 && buf.Data != IntPtr.Zero) Marshal.Copy(buf.Data, data, 0, len);
        return new BeReader(data);
    }

    private ReadOnlySpan<byte> Take(int n)
    {
        if (n < 0 || _pos + n > _data.Length)
            throw new CrossMailBindingException("read past end of rust buffer (want " + n + ")");
        var s = _data.AsSpan(_pos, n);
        _pos += n;
        return s;
    }

    public byte ReadU8() => Take(1)[0];
    public int ReadI32() => BinaryPrimitives.ReadInt32BigEndian(Take(4));
    public long ReadI64() => BinaryPrimitives.ReadInt64BigEndian(Take(8));
    public ushort ReadU16() => BinaryPrimitives.ReadUInt16BigEndian(Take(2));
    public uint ReadU32() => BinaryPrimitives.ReadUInt32BigEndian(Take(4));
    public ulong ReadU64() => BinaryPrimitives.ReadUInt64BigEndian(Take(8));
    public byte[] TakeBytes(int n) => Take(n).ToArray();

    public byte[] ReadRemaining()
    {
        var s = _data.AsSpan(_pos);
        _pos = _data.Length;
        return s.ToArray();
    }

    public void EnsureConsumed()
    {
        if (_pos != _data.Length)
            throw new CrossMailBindingException("junk data left in rust buffer: " + (_data.Length - _pos) + " bytes");
    }

    public void Dispose() { }
}

internal static class RustCall
{
    private const sbyte CallSuccess = 0;
    private const sbyte CallError = 1;
    private const sbyte CallUnexpectedError = 2;

    internal static void Check(CallStatusBox box, bool throws)
    {
        RustCallStatus st = box.Status;
        if (st.Code == CallSuccess) return;
        if (st.Code == CallError && throws)
        {
            try
            {
                using var r = BeReader.FromBuffer(st.ErrorBuf);
                throw CrossMailException.Read(r);
            }
            finally { FreeBuffer(st.ErrorBuf); }
        }
        if (st.Code == CallUnexpectedError)
        {
            string msg = "Unknown rust panic";
            if (!st.ErrorBuf.IsEmpty)
            {
                using var r = BeReader.FromBuffer(st.ErrorBuf);
                msg = Codec.ReadRawString(r);
            }
            FreeBuffer(st.ErrorBuf);
            throw new CrossMailBindingException(msg);
        }
        FreeBuffer(st.ErrorBuf);
        throw new CrossMailBindingException("CALL_ERROR without error converter, code=" + st.Code);
    }

    internal static TResult InvokeBuffer<TResult>(Func<CallStatusBox, RustBuffer> call,
        Func<BeReader, TResult> lift, bool throws)
    {
        var box = new CallStatusBox();
        RustBuffer buf = call(box);
        try
        {
            Check(box, throws);
            using var r = BeReader.FromBuffer(buf);
            TResult value = lift(r);
            r.EnsureConsumed();
            return value;
        }
        finally { FreeBuffer(buf); }
    }

    internal static T InvokeScalar<T>(Func<CallStatusBox, T> call, bool throws)
    {
        var box = new CallStatusBox();
        T value = call(box);
        Check(box, throws);
        return value;
    }

    internal static void InvokeVoid(Action<CallStatusBox> call, bool throws)
    {
        var box = new CallStatusBox();
        call(box);
        Check(box, throws);
    }

    internal static void FreeBuffer(RustBuffer buf)
    {
        if (buf.Data == IntPtr.Zero) return;
        var box = new CallStatusBox();
        NativeMethods.ffi_crossmail_ffi_rustbuffer_free(buf, ref box.Status);
    }

    // UniFFI 对象方法的 scaffolding 通过 Arc::from_raw 接管一份引用，方法返回时 drop 掉。
    // 因此每次方法调用都必须先用 clone_* 把引用计数 +1，再把这个 clone 指针交给方法；
    // 否则第一次调用就把 Arc 释放，后续调用全是 use-after-free（表现为 SQLite error 21 / 段错误）。
    // 与 Python 绑定的 self._uniffi_clone_pointer() 一一对应；clone 指针由 Rust 侧消费，C# 不释放。
    internal static IntPtr CloneEngine(CrossMailEngineHandle handle)
    {
        bool added = false;
        handle.DangerousAddRef(ref added);
        try
        {
            var box = new CallStatusBox();
            IntPtr clone = NativeMethods.uniffi_crossmail_ffi_fn_clone_crossmailengine(
                handle.DangerousGetHandle(), ref box.Status);
            Check(box, false);
            return clone;
        }
        finally { if (added) handle.DangerousRelease(); }
    }

    internal static IntPtr CloneSubscription(PushSubscriptionHandle handle)
    {
        bool added = false;
        handle.DangerousAddRef(ref added);
        try
        {
            var box = new CallStatusBox();
            IntPtr clone = NativeMethods.uniffi_crossmail_ffi_fn_clone_pushsubscription(
                handle.DangerousGetHandle(), ref box.Status);
            Check(box, false);
            return clone;
        }
        finally { if (added) handle.DangerousRelease(); }
    }
}

internal static class Codec
{
    // UniFFI: String.lower()/lift() 用裸 UTF-8 字节（无长度前缀），与嵌套 write()/read() 不同。
    // RustBuffer 一律走 rustbuffer_alloc + 直接写入，与 Python 绑定 alloc_with_builder() 同构；
    // 不走 rustbuffer_from_bytes（ForeignBytes 跨 ABI 传入会破坏 Rust 堆，表现为延迟崩溃）。
    public static RustBuffer LowerString(string value)
    {
        if (value is null) throw new ArgumentNullException(nameof(value));
        return LowerBytes(Encoding.UTF8.GetBytes(value));
    }

    public static string ReadRawString(BeReader r) => Encoding.UTF8.GetString(r.ReadRemaining());

    internal static RustBuffer Lower(Action<BeWriter> write)
    {
        using var w = new BeWriter();
        write(w);
        return LowerBytes(w.ToArray());
    }

    private static RustBuffer LowerBytes(byte[] bytes)
    {
        ulong capacity = (ulong)Math.Max(bytes.Length, 16);
        var box = new CallStatusBox();
        RustBuffer buf = NativeMethods.ffi_crossmail_ffi_rustbuffer_alloc(capacity, ref box.Status);
        RustCall.Check(box, false);
        buf.Len = (ulong)bytes.Length;
        if (bytes.Length > 0 && buf.Data != IntPtr.Zero)
            Marshal.Copy(bytes, 0, buf.Data, bytes.Length);
        return buf;
    }

    public static void WriteString(BeWriter w, string v)
    {
        byte[] b = Encoding.UTF8.GetBytes(v ?? "");
        w.WriteI32(b.Length);
        w.WriteRaw(b);
    }

    public static string ReadString(BeReader r)
    {
        int len = r.ReadI32();
        if (len < 0) throw new CrossMailBindingException("negative string length");
        return Encoding.UTF8.GetString(r.TakeBytes(len));
    }

    public static void WriteOptionalString(BeWriter w, string? v)
    {
        if (v is null) { w.WriteU8(0); return; }
        w.WriteU8(1);
        WriteString(w, v);
    }

    public static string? ReadOptionalString(BeReader r)
        => r.ReadU8() == 0 ? null : ReadString(r);

    public static void WriteBool(BeWriter w, bool v) => w.WriteU8((byte)(v ? 1 : 0));
    public static bool ReadBool(BeReader r) => r.ReadU8() != 0;

    public static void WriteU8(BeWriter w, byte v) => w.WriteU8(v);
    public static byte ReadU8(BeReader r) => r.ReadU8();
    public static void WriteOptionalU8(BeWriter w, byte? v) => WriteOptionalValue(w, v, (bw, x) => bw.WriteU8(x));
    public static byte? ReadOptionalU8(BeReader r) => ReadOptionalValue(r, br => br.ReadU8());

    public static void WriteU16(BeWriter w, ushort v) => w.WriteU16(v);
    public static ushort ReadU16(BeReader r) => r.ReadU16();
    public static void WriteOptionalU16(BeWriter w, ushort? v) => WriteOptionalValue(w, v, (bw, x) => bw.WriteU16(x));
    public static ushort? ReadOptionalU16(BeReader r) => ReadOptionalValue(r, br => br.ReadU16());

    public static void WriteU32(BeWriter w, uint v) => w.WriteU32(v);
    public static uint ReadU32(BeReader r) => r.ReadU32();
    public static void WriteOptionalU32(BeWriter w, uint? v) => WriteOptionalValue(w, v, (bw, x) => bw.WriteU32(x));
    public static uint? ReadOptionalU32(BeReader r) => ReadOptionalValue(r, br => br.ReadU32());

    public static void WriteU64(BeWriter w, ulong v) => w.WriteU64(v);
    public static ulong ReadU64(BeReader r) => r.ReadU64();
    public static void WriteOptionalU64(BeWriter w, ulong? v) => WriteOptionalValue(w, v, (bw, x) => bw.WriteU64(x));
    public static ulong? ReadOptionalU64(BeReader r) => ReadOptionalValue(r, br => br.ReadU64());

    public static void WriteI32(BeWriter w, int v) => w.WriteI32(v);
    public static int ReadI32(BeReader r) => r.ReadI32();
    public static void WriteI64(BeWriter w, long v) => w.WriteI64(v);
    public static long ReadI64(BeReader r) => r.ReadI64();

    public static void WriteEnum(BeWriter w, int v) => w.WriteI32(v);
    public static int ReadEnum(BeReader r) => r.ReadI32();

    public static void WriteOptionalEnum(BeWriter w, int? v)
        => WriteOptionalValue(w, v, (bw, x) => bw.WriteI32(x));

    public static T? ReadOptionalEnum<T>(BeReader r) where T : struct, Enum
    {
        int? raw = ReadOptionalValue(r, br => br.ReadI32());
        return raw.HasValue ? (T)Enum.ToObject(typeof(T), raw.Value) : null;
    }

    public static void WriteTimestamp(BeWriter w, DateTimeOffset v)
    {
        long seconds = v.ToUnixTimeSeconds();
        uint nanos = (uint)((v.Ticks % TimeSpan.TicksPerSecond) * 100);
        w.WriteI64(seconds);
        w.WriteU32(nanos);
    }

    public static DateTimeOffset ReadTimestamp(BeReader r)
    {
        long seconds = r.ReadI64();
        uint nanos = r.ReadU32();
        return DateTimeOffset.FromUnixTimeSeconds(seconds).AddTicks(nanos / 100);
    }

    public static void WriteOptionalTimestamp(BeWriter w, DateTimeOffset? v)
        => WriteOptionalValue(w, v, (bw, x) => WriteTimestamp(bw, x));

    public static DateTimeOffset? ReadOptionalTimestamp(BeReader r)
        => ReadOptionalValue(r, br => ReadTimestamp(br));

    public static void WriteSequence<T>(BeWriter w, IReadOnlyList<T>? items, Action<BeWriter, T> write)
    {
        int count = items?.Count ?? 0;
        w.WriteI32(count);
        if (items is null) return;
        for (int i = 0; i < count; i++) write(w, items[i]);
    }

    public static IReadOnlyList<T> ReadSequence<T>(BeReader r, Func<BeReader, T> read)
    {
        int count = r.ReadI32();
        if (count < 0) throw new CrossMailBindingException("negative sequence length");
        var list = new List<T>(count);
        for (int i = 0; i < count; i++) list.Add(read(r));
        return list;
    }

    // 值类型可选项（struct 约束）与引用类型可选项（class 约束）必须不同名，C# 不允许仅约束不同的重载。
    private static void WriteOptionalValue<T>(BeWriter w, T? value, Action<BeWriter, T> write) where T : struct
    {
        if (!value.HasValue) { w.WriteU8(0); return; }
        w.WriteU8(1);
        write(w, value.Value);
    }

    private static T? ReadOptionalValue<T>(BeReader r, Func<BeReader, T> read) where T : struct
    {
        byte flag = r.ReadU8();
        if (flag == 0) return null;
        if (flag != 1) throw new CrossMailBindingException("unexpected optional flag: " + flag);
        return read(r);
    }

    public static void WriteOptional<T>(BeWriter w, T? value, Action<BeWriter, T> write) where T : class
    {
        if (value is null) { w.WriteU8(0); return; }
        w.WriteU8(1);
        write(w, value);
    }

    public static T? ReadOptional<T>(BeReader r, Func<BeReader, T> read) where T : class
    {
        byte flag = r.ReadU8();
        if (flag == 0) return null;
        if (flag != 1) throw new CrossMailBindingException("unexpected optional flag: " + flag);
        return read(r);
    }
}

internal sealed class CrossMailEngineHandle : SafeHandle
{
    public CrossMailEngineHandle(IntPtr ptr) : base(IntPtr.Zero, true) => SetHandle(ptr);
    public override bool IsInvalid => handle == IntPtr.Zero;

    protected override bool ReleaseHandle()
    {
        var box = new CallStatusBox();
        NativeMethods.uniffi_crossmail_ffi_fn_free_crossmailengine(handle, ref box.Status);
        return true;
    }
}

internal sealed class PushSubscriptionHandle : SafeHandle
{
    public PushSubscriptionHandle(IntPtr ptr) : base(IntPtr.Zero, true) => SetHandle(ptr);
    public override bool IsInvalid => handle == IntPtr.Zero;

    protected override bool ReleaseHandle()
    {
        var box = new CallStatusBox();
        NativeMethods.uniffi_crossmail_ffi_fn_free_pushsubscription(handle, ref box.Status);
        return true;
    }
}
