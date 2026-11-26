using System.Collections.Generic;
using System.Threading.Tasks;
using CrossMail.Bindings;

namespace CrossMail.App.Services;

// 设置聚合页的服务层：所有 UDL 同步阻塞调用统一转后台线程，异常原文向上抛。
// 不缓存 engine、不持有 SafeHandle、不吞错、不填假数据。
public static class SettingsService
{
    private static CrossMailEngine Open() => CrossMailCore.OpenEngine(CoreBridge.CreateConfig());

    public static Task<CoreStatus> CoreStatusAsync() =>
        CoreBridge.RunAsync(() => CrossMailCore.CoreStatus());

    public static Task<CacheStatusDto> CacheStatusAsync() => CoreBridge.RunAsync(() =>
    {
        using var e = Open();
        return e.CacheStatus();
    });

    public static Task<EvictionReportDto> EnforceQuotaAsync() => CoreBridge.RunAsync(() =>
    {
        using var e = Open();
        return e.EnforceCacheQuota();
    });

    public static Task<uint> ClearCacheAsync() => CoreBridge.RunAsync(() =>
    {
        using var e = Open();
        return e.ClearCache();
    });

    public static Task<LogStatusDto> LogStatusAsync() => CoreBridge.RunAsync(() =>
    {
        using var e = Open();
        return e.LogStatus();
    });

    public static Task<string> SetLogLevelAsync(string level) => CoreBridge.RunAsync(() =>
    {
        using var e = Open();
        return e.SetLogLevel(level);
    });

    public static Task<bool> RotateLogsAsync() => CoreBridge.RunAsync(() =>
    {
        using var e = Open();
        return e.RotateLogs();
    });

    public static Task<string> ExportDiagnosticsAsync(string dir) => CoreBridge.RunAsync(() =>
    {
        using var e = Open();
        return e.ExportDiagnostics(dir);
    });

    public static Task<IReadOnlyList<AllowanceDto>> AllowancesAsync() => CoreBridge.RunAsync(() =>
    {
        using var e = Open();
        return e.ListRemoteAllowances();
    });

    public static Task SetProtocolPayloadLoggingAsync(bool enabled) => CoreBridge.RunAsync(() =>
    {
        using var e = Open();
        e.SetProtocolPayloadLogging(enabled);
        return true;
    });
}
