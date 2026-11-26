using System;
using System.IO;
using System.Threading.Tasks;
using CrossMail.App.Services;
using Microsoft.UI.Xaml.Controls;

namespace CrossMail.App.Pages;

public sealed class InitPage : CorePage
{
    public InitPage() : base("初始化", "S5-02")
    {
        Root.Children.Add(Action("读取核心状态（真调用）", Reload));
        Root.Children.Add(Action("初始化本地数据目录", InitDir));
        _ = Reload();
    }

    private async Task Reload()
    {
        var s = await GuardAsync("core_status / open_engine", CoreBridge.GetSnapshotAsync);
        if (s is null) return;
        string dll = Path.Combine(AppContext.BaseDirectory, "crossmail_ffi.dll");
        Show(string.Join(Environment.NewLine,
            "core_status.version                = " + s.Version,
            "core_status.supported_protocols    = " + s.Protocols,
            "open_engine                        = " + s.EngineType,
            "list_accounts.count                = " + s.AccountCount,
            "cache_status.usage_bytes           = " + s.CacheUsageBytes,
            "cache_status.quota_bytes           = " + s.CacheQuotaBytes,
            "data_root                          = " + CoreBridge.DataRoot,
            "dll_path                           = " + dll,
            "dll_exists / bytes                 = " + File.Exists(dll) + " / " + (File.Exists(dll) ? new FileInfo(dll).Length : 0)));
    }

    private async Task InitDir()
    {
        var config = await Guard("create_config", CoreBridge.CreateConfig);
        if (config is null) return;
        Show(string.Join(Environment.NewLine,
            "db_path       = " + config.DbPath,
            "cache_dir     = " + config.CacheDir,
            "quota_bytes   = " + config.CacheQuotaBytes,
            "master_key    = (未设置)"));
    }
}

public sealed class PackagingPage : CorePage
{
    public PackagingPage() : base("打包 (x64 / ARM64)", "S5-08")
    {
        Root.Children.Add(Action("扫描本机打包产物", Scan));
        _ = Scan();
    }

    private async Task Scan()
    {
        var report = await Guard("scan artifacts", () =>
        {
            string baseDir = AppContext.BaseDirectory;
            string dll = Path.Combine(baseDir, "crossmail_ffi.dll");
            string arm64 = Path.Combine(baseDir, "arm64", "crossmail_ffi.dll");
            return string.Join(Environment.NewLine,
                "output_dir            = " + baseDir,
                "x64  crossmail_ffi.dll= " + (File.Exists(dll) ? new FileInfo(dll).Length + " bytes" : "缺失"),
                "ARM64 crossmail_ffi   = " + (File.Exists(arm64) ? new FileInfo(arm64).Length + " bytes" : "未产出"),
                "ARM64 解除条件         = 需 aarch64-pc-windows-msvc Rust 目标产出 DLL；本机仅编译 x86_64-pc-windows-msvc",
                "MSIX 状态              = Step5 验证链输出，未验证前不声称已打包");
        });
        if (report is not null) Show(report);
    }
}
