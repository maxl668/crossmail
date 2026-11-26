using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading.Tasks;
using CrossMail.App.Services;
using CrossMail.Bindings;
using Microsoft.UI.Xaml.Controls;

namespace CrossMail.App.Pages;

public sealed class AccountsPage : CorePage
{
    private readonly TextBox _email = Field("邮箱地址", "user@example.com");
    private readonly TextBox _display = Field("显示名（可选）", "");
    private readonly TextBox _secret = Field("Secret（仅本机，不落日志）", "");
    private readonly TextBox _endpoint = Field("JMAP Endpoint / IMAP Host", "mail.example.com（IMAP 只填主机名，不带 http:// 和端口）");
    private readonly TextBox _smtp = Field("SMTP Host（可选）", "smtp.example.com");
    private readonly TextBox _accountId = Field("账号 ID（删除/更新用）", "");
    private readonly ComboBox _protocol = new();
    // Rust new_account_from_dto -> endpoint_from_dto 强制要求端口与加密方式，
    // 缺任一项会直接报「IMAP 端口不能为空 / IMAP 加密方式不能为空」。
    // 这里预填隐式 TLS 的常用值，让 IMAP 账号只需填 3 个字段即可创建。
    private readonly TextBox _imapPort = ValueField("IMAP 端口（隐式 TLS 用 993）", "993", "993");
    private readonly TextBox _smtpPort = ValueField("SMTP 端口（隐式 TLS 用 465）", "465", "465");
    private readonly ComboBox _imapSecurity = SecurityBox();
    private readonly ComboBox _smtpSecurity = SecurityBox();

    private static TextBox ValueField(string header, string placeholder, string value)
    {
        var box = Field(header, placeholder);
        box.Text = value;
        return box;
    }

    private static ComboBox SecurityBox()
    {
        var box = new ComboBox { MinWidth = 240 };
        box.Items.Add("隐式 TLS (ImplicitTls)");
        box.Items.Add("STARTTLS (StartTls，核心暂不支持)");
        box.SelectedIndex = 0;
        return box;
    }

    public AccountsPage() : base("账号", "S5-03")
    {
        _protocol.Items.Add("JMAP");
        _protocol.Items.Add("IMAP");
        // 真机唯一可达通道是 IMAPS 993（8085 JMAP 仅监听 Self-hosted server 127.0.0.1）。
        // 默认选 IMAP，避免建出 Windows 侧连不上的 JMAP 账号。
        _protocol.SelectedIndex = 1;
        Root.Children.Add(Row(_email, _display));
        Root.Children.Add(Row(_protocol, _endpoint));
        Root.Children.Add(Row(_secret, _smtp));
        Root.Children.Add(Row(_imapPort, _imapSecurity));
        Root.Children.Add(Row(_smtpPort, _smtpSecurity));
        Root.Children.Add(Row(_accountId));
        Root.Children.Add(Row(
            Action("新建账号 (create_account)", Create),
            Action("刷新列表 (list_accounts)", Refresh),
            Action("删除账号 (delete_account)", Delete),
            Action("更新同步天数 (update_account)", Patch)));
        _ = Refresh();
    }

    // IMAP/SMTP 只接受主机名；用户从网页版复制地址时经常带 http:// 和端口，
    // 这里在提交前规范化，避免把 "http://host" 当成 TLS 主机名导致握手失败。
    private static string NormalizeHost(string raw)
    {
        string s = raw.Trim();
        if (s.Length == 0) return s;
        if (s.Contains("://", StringComparison.Ordinal) &&
            Uri.TryCreate(s, UriKind.Absolute, out var uri) && !string.IsNullOrEmpty(uri.Host))
            s = uri.Host;
        int slash = s.IndexOf('/');
        if (slash >= 0) s = s[..slash];
        int colon = s.LastIndexOf(':');
        if (colon > 0 && s.LastIndexOf(']') < colon) s = s[..colon];
        return s;
    }

    private async Task Create()
    {
        // 输入先在 UI 线程读好，后台线程不得触碰控件。
        string email = _email.Text, display = _display.Text, secret = _secret.Text;
        string endpoint = _endpoint.Text, smtp = _smtp.Text;
        bool imap = _protocol.SelectedIndex == 1;
        string host = NormalizeHost(endpoint);
        string smtpHost = NormalizeHost(smtp);
        bool useSmtp = !string.IsNullOrWhiteSpace(smtp);
        ushort? imapPort = ushort.TryParse(_imapPort.Text.Trim(), out var ip) ? ip : null;
        ushort? smtpPort = ushort.TryParse(_smtpPort.Text.Trim(), out var sp) ? sp : null;
        SecurityModeDto Sec(ComboBox box) => box.SelectedIndex == 1
            ? SecurityModeDto.StartTls
            : SecurityModeDto.ImplicitTls;

        var input = new NewAccountDto
        {
            EmailAddress = email,
            DisplayName = string.IsNullOrWhiteSpace(display) ? null : display,
            Protocol = imap ? ProtocolKindDto.Imap : ProtocolKindDto.Jmap,
            Secret = secret,
            JmapEndpoint = imap ? null : endpoint,
            ImapHost = imap ? host : null,
            ImapPort = imap ? imapPort : null,
            ImapSecurity = imap ? Sec(_imapSecurity) : (SecurityModeDto?)null,
            SmtpHost = useSmtp ? smtpHost : null,
            SmtpPort = useSmtp ? smtpPort : null,
            SmtpSecurity = useSmtp ? Sec(_smtpSecurity) : (SecurityModeDto?)null,
        };
        var acc = await WithEngine("create_account", e => e.CreateAccount(input));
        if (acc is null) return;
        _accountId.Text = acc.Id;
        Show("account.id=" + acc.Id + "\nemail=" + acc.EmailAddress +
             "\nprotocol=" + acc.Protocol + "\nsync_days=" + acc.SyncDays);
    }

    private async Task Refresh()
    {
        var list = await WithEngine("list_accounts", e => e.ListAccounts());
        if (list is null) return;
        Show(list.Count == 0
            ? "list_accounts.count = 0（本机数据库尚无账号）"
            : string.Join(Environment.NewLine, list.Select(a =>
                $"{a.Id} | {a.EmailAddress} | {a.Protocol} | sync_days={a.SyncDays} | poll={a.PollIntervalSecs}s")));
    }

    private async Task Delete()
    {
        string id = _accountId.Text;
        await WithEngine("delete_account", e => { e.DeleteAccount(id); return true; });
        await Refresh();
    }

    private async Task Patch()
    {
        string id = _accountId.Text;
        var patch = new AccountPatchDto { SyncDays = 30U, PollIntervalSecs = 60U };
        var acc = await WithEngine("update_account", e => e.UpdateAccount(id, patch));
        if (acc is null) return;
        Show("updated id=" + acc.Id + " sync_days=" + acc.SyncDays + " poll=" + acc.PollIntervalSecs);
    }
}

public sealed class SettingsPage : CorePage
{
    private readonly ComboBox _level = new();
    private readonly TextBox _diagDir = Field("诊断导出目录", "");

    public SettingsPage() : base("设置", "S5-07")
    {
        foreach (string s in new[] { "trace", "debug", "info", "warn", "error" }) _level.Items.Add(s);
        _level.SelectedIndex = 2;
        Root.Children.Add(Row(_level, _diagDir));
        Root.Children.Add(Row(
            Action("缓存状态 (cache_status)", Cache),
            Action("强制配额 (enforce_cache_quota)", Quota),
            Action("清空缓存 (clear_cache)", Clear)));
        Root.Children.Add(Row(
            Action("设置日志级别 (set_log_level)", SetLevel),
            Action("日志状态 (log_status)", LogStatus),
            Action("轮转日志 (rotate_logs)", Rotate)));
        Root.Children.Add(Row(
            Action("远程图片授权 (list_remote_allowances)", Allowances),
            Action("导出诊断 (export_diagnostics)", Export),
            Action("协议载荷日志开关 (set_protocol_payload_logging)", Payload)));
        _ = Cache();
    }

    private async Task Cache()
    {
        var s = await WithEngine("cache_status", e => e.CacheStatus());
        if (s is not null)
            Show($"usage_bytes={s.UsageBytes}\nquota_bytes={s.QuotaBytes}\nover_quota={s.OverQuota}");
    }

    private async Task Quota()
    {
        var s = await WithEngine("enforce_cache_quota", e => e.EnforceCacheQuota());
        if (s is not null)
            Show($"removed_count={s.RemovedCount}\nfreed_bytes={s.FreedBytes}\nusage_after={s.UsageAfter}\nforeign_entries={s.ForeignEntries}");
    }

    private async Task Clear()
    {
        var n = await WithEngine("clear_cache", e => e.ClearCache());
        Show("clear_cache = " + n);
    }

    private async Task SetLevel()
    {
        string level = (string)_level.SelectedItem;
        var s = await WithEngine("set_log_level", e => e.SetLogLevel(level));
        Show("set_log_level = " + s);
    }

    private async Task LogStatus()
    {
        var s = await WithEngine("log_status", e => e.LogStatus());
        if (s is not null)
            Show($"level={s.Level}\nprotocol_payload={s.ProtocolPayload}\ndir={s.Dir}\nfiles={string.Join(",", s.Files)}");
    }

    private async Task Rotate()
    {
        var b = await WithEngine("rotate_logs", e => e.RotateLogs());
        Show("rotate_logs = " + b);
    }

    private async Task Allowances()
    {
        var list = await WithEngine("list_remote_allowances", e => e.ListRemoteAllowances());
        if (list is not null)
            Show(list.Count == 0 ? "allowances.count = 0" :
                string.Join(Environment.NewLine, list.Select(a => $"{a.Sender} | {a.Scope} | {a.ExpiresAt}")));
    }

    private async Task Export()
    {
        string dir = string.IsNullOrWhiteSpace(_diagDir.Text) ? CoreBridge.DataRoot : _diagDir.Text;
        var p = await WithEngine("export_diagnostics", e => e.ExportDiagnostics(dir));
        Show("export_diagnostics = " + p);
    }

    private async Task Payload()
    {
        await WithEngine("set_protocol_payload_logging", e => { e.SetProtocolPayloadLogging(false); return true; });
        var s = await WithEngine("log_status", e => e.LogStatus());
        Show("protocol_payload = " + (s?.ProtocolPayload.ToString() ?? "(unknown)"));
    }
}
