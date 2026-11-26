using System;
using System.Runtime.InteropServices;
using System.Text;
using Microsoft.UI.Xaml;

namespace CrossMail.App.Services;

/// <summary>
/// 新邮件托盘气泡提醒（v1.7，FFI/WinUI 3 绿色版）。
/// 绿色版无 package identity，系统 Toast 需 AUMID + COM activator，因此用
/// Win32 Shell_NotifyIconW 在独立 uID 上弹气泡，不干扰其他托盘图标。
/// 所有失败静默——提醒不能影响收信主链路。
/// </summary>
public static class MailNotifier
{
    private const uint BubbleUid = 0xC0FFEE;
    private const uint WmBubble = 0x0800 + 96;
    private const uint NifMessage = 0x0001;
    private const uint NifIcon = 0x0002;
    private const uint NifTip = 0x0004;
    private const uint NifInfo = 0x0010;
    private const uint NiifInfo = 0x00000001;
    private const uint NimAdd = 0x00000000;
    private const uint NimModify = 0x00000001;
    private const uint NimDelete = 0x00000002;

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    private struct NotifyIconData
    {
        public uint cbSize;
        public IntPtr hWnd;
        public uint uID;
        public uint uFlags;
        public uint uCallbackMessage;
        public IntPtr hIcon;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)] public string szTip;
        public uint dwState;
        public uint dwStateMask;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 256)] public string szInfo;
        public uint uTimeoutOrVersion;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 64)] public string szInfoTitle;
        public uint dwInfoFlags;
        public Guid guidItem;
        public IntPtr hBalloonIcon;
    }

    [DllImport("shell32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern bool Shell_NotifyIconW(uint dwMessage, ref NotifyIconData lpData);

    [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern IntPtr LoadIcon(IntPtr hInstance, IntPtr lpIconName);

    [DllImport("user32.dll")]
    private static extern IntPtr GetActiveWindow();

    private static NotifyIconData BuildNid(IntPtr hwnd) => new()
    {
        cbSize = (uint)Marshal.SizeOf<NotifyIconData>(),
        hWnd = hwnd,
        uID = BubbleUid,
        uCallbackMessage = WmBubble,
        szTip = string.Empty,
        szInfo = string.Empty,
        szInfoTitle = string.Empty,
    };

    private static IntPtr GetHwnd(Window? window)
    {
        // 回调已切到 UI 线程，GetActiveWindow 即主窗口 HWND；无需 WindowNative 互操作引用。
        try { return GetActiveWindow(); }
        catch { return IntPtr.Zero; }
    }

    /// <summary>弹新邮件托盘气泡。window 为null时静默跳过。</summary>
    public static void NotifyNewMail(Window? window, ulong inserted, ulong unreadTotal)
    {
        if (window is null) return;
        try
        {
            var hwnd = GetHwnd(window);
            if (hwnd == IntPtr.Zero) return;

            var nid = BuildNid(hwnd);
            nid.uFlags = NifMessage | NifIcon | NifTip;
            nid.hIcon = LoadIcon(IntPtr.Zero, new IntPtr(32512)); // IDI_APPLICATION
            nid.szTip = "CrossMail 新邮件提醒";
            Shell_NotifyIconW(NimAdd, ref nid); // 已存在则失败无妨，后续 MODIFY 仍命中

            nid.uFlags = NifInfo;
            nid.dwInfoFlags = NiifInfo;
            nid.szInfoTitle = "CrossMail · 新邮件";
            nid.szInfo = $"收到 {inserted} 封新邮件，未读共 {unreadTotal} 封";
            nid.uTimeoutOrVersion = 15_000;
            Shell_NotifyIconW(NimModify, ref nid);
        }
        catch { /* 提醒失败不影响主链路 */ }
    }

    /// <summary>清理气泡图标（退出时调用）。</summary>
    public static void Cleanup(Window? window)
    {
        if (window is null) return;
        try
        {
            var hwnd = GetHwnd(window);
            if (hwnd == IntPtr.Zero) return;
            var nid = BuildNid(hwnd);
            Shell_NotifyIconW(NimDelete, ref nid);
        }
        catch { }
    }
}
