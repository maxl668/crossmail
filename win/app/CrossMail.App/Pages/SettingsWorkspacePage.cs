using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading.Tasks;
using CrossMail.App.Services;
using CrossMail.Bindings;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace CrossMail.App.Pages;

// 参考 Bulwark Webmail /settings 的分段式设置，但只暴露 Windows 核心真实具备的能力。
// 五段：账号与同步 / 缓存 / 隐私 / 日志与诊断 / 关于（能力边界）。
// 每个按钮都对应一次真实 FFI 调用；危险操作必须先勾选确认，且从不进入自动化 smoke。
public sealed class SettingsWorkspacePage : Page
{
    public sealed record SettingsAction(string Label, Func<Task> Handler);

    private readonly StackPanel _root = new() { Margin = new Thickness(20, 14, 20, 14), Spacing = 10 };
    private readonly List<UIElement> _sections = new();
    private readonly List<SettingsAction> _safeActions = new();
    private readonly TextBlock _output = new() { TextWrapping = TextWrapping.Wrap, IsTextSelectionEnabled = true };
    private readonly TextBlock _head = new() { FontSize = 22, Text = "设置" };
    private readonly ComboBox _level = new() { MinWidth = 140 };
    private readonly CheckBox _confirm = new() { Content = "我确认执行缓存清理 / 日志轮转 / 导出诊断等写操作" };
    private readonly TextBox _diagDir = new()
    {
        Header = "诊断导出目录（留空用默认应用数据目录）",
        PlaceholderText = "C:\\...",
        MinWidth = 260,
    };

    private int _safeCount;
    private int _guardedCount;

    public int SectionCount => _sections.Count;
    public int SafeActionCount => _safeActions.Count;
    public int GuardedActionCount => _guardedCount;
    public int ExecutedActions { get; private set; }
    public string LastOutput => _output.Text;
    public IReadOnlyList<SettingsAction> SafeActions => _safeActions;

    public SettingsWorkspacePage()
    {
        foreach (string s in new[] { "trace", "debug", "info", "warn", "error" }) _level.Items.Add(s);
        _level.SelectedIndex = 2;

        _root.Children.Add(_head);
        _root.Children.Add(new TextBlock
        {
            Text = "所有数值均来自本机 Rust 引擎真调用；未接核心的能力不显示为开关。",
            Opacity = 0.7,
            TextWrapping = TextWrapping.Wrap,
        });

        _sections.Add(AddSection("账号与同步", "账号列表、当前账号同步、核心版本。凭据不在此页展示。",
            SafeButton("核心版本 (core_status)", CoreStatus),
            SafeButton("账号列表 (list_accounts)", Accounts),
            SafeButton("同步当前账号 (sync_account)", SyncCurrent)));

        _sections.Add(AddSection("缓存", "本地 SQLite + 文件缓存配额。清理类操作会真实删除缓存条目。",
            SafeButton("缓存状态 (cache_status)", CacheStatus),
            GuardedButton("强制配额清理 (enforce_cache_quota)", EnforceQuota),
            GuardedButton("清空缓存 (clear_cache)", ClearCache)));

        _sections.Add(AddSection("隐私", "远程图片发件人授权、协议载荷日志。默认不记录邮件正文载荷。",
            SafeButton("远程图片授权 (list_remote_allowances)", Allowances),
            SafeButton("关闭协议载荷日志 (set_protocol_payload_logging=false)", DisablePayloadLog)));

        _sections.Add(AddSection("日志与诊断", "日志级别、状态、轮转与诊断导出。",
            Row(_level, SafeButton("设置日志级别 (set_log_level)", SetLevel),
                SafeButton("日志状态 (log_status)", LogStatus)),
            Row(GuardedButton("轮转日志 (rotate_logs)", RotateLogs), _diagDir,
                GuardedButton("导出诊断 (export_diagnostics)", Export))));

        _sections.Add(AddSection("关于 / 能力边界",
            "日历、联系人、文件、插件在网页版 Bulwark 中存在，但当前 Rust 核心对应 FFI 方法数为 0；" +
            "本客户端不提供假界面。完整验收台见左侧「工程验收」。",
            new TextBlock { Text = "无可用操作（诚实占位段）", Opacity = 0.65 }));

        _root.Children.Add(_confirm);
        _root.Children.Add(new TextBlock { Text = "最近一次结果", Opacity = 0.65 });
        _root.Children.Add(_output);
        Content = new ScrollViewer { Content = _root };

        _ = SafeExec("core_status", CoreStatus);
    }

    private UIElement AddSection(string title, string description, params UIElement[] children)
    {
        var border = new Border
        {
            BorderThickness = new Thickness(1),
            BorderBrush = new Microsoft.UI.Xaml.Media.SolidColorBrush(Microsoft.UI.Colors.LightGray),
            CornerRadius = new CornerRadius(6),
            Padding = new Thickness(14, 10, 14, 12),
        };
        var panel = new StackPanel { Spacing = 8 };
        panel.Children.Add(new TextBlock { Text = title, FontSize = 16 });
        panel.Children.Add(new TextBlock { Text = description, Opacity = 0.7, TextWrapping = TextWrapping.Wrap });
        foreach (var c in children) panel.Children.Add(c);
        border.Child = panel;
        _root.Children.Add(border);
        return border;
    }

    private static StackPanel Row(params UIElement[] items)
    {
        var p = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        foreach (var i in items) p.Children.Add(i);
        return p;
    }

    private Button SafeButton(string label, Func<Task<string>> handler)
    {
        var b = new Button { Content = label };
        b.Click += (_, _) => _ = SafeExec(label, handler);
        _safeActions.Add(new SettingsAction(label, () => SafeExec(label, handler)));
        _safeCount++;
        return b;
    }

    private Button GuardedButton(string label, Func<Task<string>> handler)
    {
        var b = new Button { Content = label };
        b.Click += (_, _) => _ = GuardedExec(label, handler);
        _guardedCount++;
        return b;
    }

    private async Task SafeExec(string label, Func<Task<string>> handler)
    {
        try { Show("[" + label + "] " + await handler()); }
        catch (Exception ex) { Show("[" + label + "] 真实错误：" + ex); }
    }

    private async Task GuardedExec(string label, Func<Task<string>> handler)
    {
        if (_confirm.IsChecked != true)
        {
            Show("[" + label + "] 未执行：需先勾选确认（防止误删缓存 / 误写文件）。");
            return;
        }
        await SafeExec(label, handler);
    }

    private void Show(string text)
    {
        _output.Text = text;
        ExecutedActions++;
    }

    private static async Task<string> CoreStatus()
    {
        var s = await SettingsService.CoreStatusAsync();
        return "version=" + s.Version + " | protocols=" + string.Join(",", s.SupportedProtocols);
    }

    private static async Task<string> Accounts()
    {
        var accounts = await MailService.AccountsAsync();
        return "accounts=" + accounts.Count +
               (accounts.Count == 0 ? "（本机尚无账号，同步不可用）" :
                   " | " + string.Join(" ; ", accounts.Select(a => a.EmailAddress)));
    }

    private async Task<string> SyncCurrent()
    {
        string? id = AppState.Current.AccountId;
        if (string.IsNullOrEmpty(id)) return "未执行：当前没有账号（list_accounts.count = 0 或未选择）。";
        var r = await MailService.SyncAsync(id);
        return "inserted=" + r.Inserted + " updated=" + r.Updated + " deleted=" + r.Deleted +
               " unchanged=" + r.Unchanged + " has_more=" + r.HasMore;
    }

    private async Task<string> CacheStatus()
    {
        var s = await SettingsService.CacheStatusAsync();
        return "usage_bytes=" + s.UsageBytes + " | quota_bytes=" + s.QuotaBytes + " | over_quota=" + s.OverQuota;
    }

    private async Task<string> EnforceQuota()
    {
        var r = await SettingsService.EnforceQuotaAsync();
        return "removed_count=" + r.RemovedCount + " | freed_bytes=" + r.FreedBytes +
               " | usage_after=" + r.UsageAfter + " | foreign_entries=" + r.ForeignEntries;
    }

    private async Task<string> ClearCache()
    {
        uint n = await SettingsService.ClearCacheAsync();
        return "clear_cache removed=" + n;
    }

    private async Task<string> Allowances()
    {
        var list = await SettingsService.AllowancesAsync();
        return list.Count == 0 ? "allowances.count = 0（尚无远程图片授权）" :
            "allowances=" + string.Join(" ; ", list.Select(a => a.Sender + " | " + a.Scope + " | " + a.ExpiresAt));
    }

    private async Task<string> DisablePayloadLog()
    {
        await SettingsService.SetProtocolPayloadLoggingAsync(false);
        var s = await SettingsService.LogStatusAsync();
        return "set_protocol_payload_logging(false) -> protocol_payload=" + s.ProtocolPayload;
    }

    private async Task<string> SetLevel()
    {
        string level = (string?)_level.SelectedItem ?? "info";
        string r = await SettingsService.SetLogLevelAsync(level);
        return "set_log_level(" + level + ") = " + r;
    }

    private async Task<string> LogStatus()
    {
        var s = await SettingsService.LogStatusAsync();
        return "level=" + s.Level + " | protocol_payload=" + s.ProtocolPayload +
               " | dir=" + (s.Dir ?? "(none)") + " | files=" + s.Files.Count;
    }

    private async Task<string> RotateLogs()
    {
        bool b = await SettingsService.RotateLogsAsync();
        return "rotate_logs = " + b;
    }

    private async Task<string> Export()
    {
        string dir = string.IsNullOrWhiteSpace(_diagDir.Text) ? CoreBridge.DataRoot : _diagDir.Text;
        string p = await SettingsService.ExportDiagnosticsAsync(dir);
        return "export_diagnostics = " + p;
    }
}
