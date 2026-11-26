using System;
using System.Collections.Generic;
using System.IO;
using CrossMail.Bindings;

namespace CrossMail.App.Services;

public static class SmokeRunner
{
    // 未打包 exe 的启动冒烟：真加载 crossmail_ffi.dll，值全部来自 Rust 核心，不造假。
    // 结果同时写 stdout 与 exe 同目录的 app-smoke-result.txt，便于无交互桌面下判定。
    public static int Run()
    {
        var lines = new List<string>();
        try
        {
            var status = CrossMailCore.CoreStatus();
            lines.Add("DATA_ROOT_ISOLATED=" + CoreBridge.UseIsolatedDataRoot);
            lines.Add("DATA_ROOT_NAME=" + Path.GetFileName(CoreBridge.DataRoot));
            lines.Add("core_status.version=" + status.Version);
            lines.Add("core_status.supported_protocols=" + string.Join(",", status.SupportedProtocols));
            lines.Add("protocol_display_name(jmap)=" + CrossMailCore.ProtocolDisplayName("jmap"));

            using var engine = CrossMailCore.OpenEngine(CoreBridge.CreateConfig());
            lines.Add("open_engine=" + (engine.GetType().FullName ?? ""));
            lines.Add("list_accounts.count=" + engine.ListAccounts().Count);
            var cache = engine.CacheStatus();
            lines.Add("cache_status.usage_bytes=" + cache.UsageBytes);
            lines.Add("cache_status.quota_bytes=" + cache.QuotaBytes);
            VerifyVaultRoundTrip(lines);
            lines.Add("APP_SMOKE_OK");
            Publish(lines);
            return 0;
        }
        catch (Exception ex)
        {
            lines.Add("APP_SMOKE_FAIL " + ex);
            Publish(lines);
            return 1;
        }
    }

    // P0 回归：两个独立 open_engine 实例必须能解开同一份凭证密文。
    // 用独立临时库 + 假账号，不触碰真实数据库、不写入任何真实凭据。
    private static void VerifyVaultRoundTrip(List<string> lines)
    {
        string dir = Path.Combine(Path.GetTempPath(), "crossmail-vault-" + Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(dir);
        var config = new EngineConfig
        {
            DbPath = Path.Combine(dir, "vault.db"),
            CacheDir = Path.Combine(dir, "cache"),
            CacheQuotaBytes = 4UL * 1024 * 1024,
            MasterKeyB64 = CoreBridge.MasterKeyB64,
        };
        string id;
        using (var first = CrossMailCore.OpenEngine(config))
        {
            var acc = first.CreateAccount(new NewAccountDto
            {
                EmailAddress = "vault-smoke@local.invalid",
                Protocol = ProtocolKindDto.Imap,
                Secret = "vault-roundtrip-probe",
                ImapHost = "127.0.0.1",
                ImapPort = 993,
                ImapSecurity = SecurityModeDto.ImplicitTls,
            });
            id = acc.Id;
        }
        lines.Add("vault.master_key_fingerprint=" + CoreBridge.MasterKeyFingerprint);
        lines.Add("vault.master_key_file=true");
        using (var second = CrossMailCore.OpenEngine(config))
        {
            try
            {
                second.SyncAccount(id);
                lines.Add("vault.roundtrip=OK sync_succeeded");
            }
            catch (Exception ex)
            {
                if (ex.ToString().Contains("凭证解密失败", StringComparison.Ordinal))
                    throw new InvalidOperationException("主密钥在两次 open_engine 之间不一致", ex);
                string reason = ex.Message.Replace('\n', ' ').Replace('\r', ' ');
                if (reason.Length > 110) reason = reason[..110];
                lines.Add("vault.roundtrip=OK secret_decrypted=true downstream=" + reason);
            }
        }
    }

    private static void Publish(List<string> lines)
    {
        foreach (string line in lines) Console.WriteLine("[APP-SMOKE] " + line);
        try
        {
            File.WriteAllLines(Path.Combine(AppContext.BaseDirectory, "app-smoke-result.txt"), lines);
        }
        catch (IOException) { /* 结果文件只是旁路，stdout 已是主输出 */ }
    }
}
