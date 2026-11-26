using System;
using System.Collections.Generic;
using System.Threading.Tasks;
using CrossMail.App.Pages;
using CrossMail.App.Services;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace CrossMail.App;

// 参考 Bulwark Webmail 的产品外壳重构：
//   顶部命令栏 + 左侧模块导航 + 主工作区 + 底部状态栏。
// S5-02~S5-09 八个验收页不再与产品模块并列，收进「工程验收」二级分区。
public sealed partial class MainWindow : Window
{
    private readonly Frame _frame = new();
    private readonly TextBox _search = new()
    {
        PlaceholderText = "搜索邮件（Search mail...）",
        Width = 300,
        VerticalAlignment = VerticalAlignment.Center,
    };
    private readonly TextBlock _shellHint = new()
    {
        Text = "",
        Opacity = 0.75,
        TextWrapping = TextWrapping.Wrap,
        VerticalAlignment = VerticalAlignment.Center,
    };
    private readonly TextBlock _statusBar = new()
    {
        Text = "核心状态读取中",
        Opacity = 0.8,
        TextWrapping = TextWrapping.Wrap,
    };
    private readonly List<Button> _commandButtons = new();
    private readonly List<string> _commandTrace = new();
    private NavigationView _nav = null!;
    private string _moduleHint = "";

    internal NavigationView Nav => _nav;
    internal Type? CurrentPageType => _frame.Content?.GetType() ?? _frame.CurrentSourcePageType;
    internal string StatusText => _statusBar.Text;
    internal int CommandButtons => _commandButtons.Count;
    internal int WorkspaceColumns => (_frame.Content as MailWorkspacePage)?.ColumnCount ?? 0;
    internal int DiagnosticPageCount => (_frame.Content as DiagnosticsPage)?.DiagnosticPages ?? Pages.Length;
    internal string SelectedTag => (_nav.SelectedItem as NavigationViewItem)?.Tag as string ?? "null";

    // 按钮级 shell smoke 用：真 UI 事件路由触发 Click 后，等待同一个 Task 完成。
    internal IReadOnlyList<Button> CommandButtonControls => _commandButtons;
    internal IReadOnlyList<string> CommandTrace => _commandTrace;
    internal Task? LastCommandTask { get; private set; }
    internal string CommandHint => _shellHint.Text;
    internal int OpenComposeCount => (_frame.Content as MailWorkspacePage)?.OpenComposeCount ?? 0;
    internal void CloseComposeWindows() => (_frame.Content as MailWorkspacePage)?.CloseComposeWindows();

    // 产品化一级导航：8 项，与既有 window-smoke 的 8 项断言兼容。
    internal static readonly (string Tag, string Title)[] ShellPages =
    {
        ("mail",        "邮件"),
        ("calendar",    "日历"),
        ("contacts",    "联系人"),
        ("files",       "文件"),
        ("accounts",    "账号"),
        ("settings",    "设置"),
        ("apps",        "应用"),
        ("diagnostics", "工程验收"),
    };

    // 验收分区：保持原有八个交付项与 sprint 编号，--ui-smoke 依赖此数组。
    internal static readonly (string Tag, string Title, string Sprint)[] Pages =
    {
        ("init",       "初始化",            "S5-02"),
        ("accounts",   "账号",              "S5-03"),
        ("maillist",   "邮件列表",          "S5-04"),
        ("reader",     "阅读",              "S5-05"),
        ("compose",    "撰写",              "S5-06"),
        ("settings",   "设置",              "S5-07"),
        ("packaging",  "打包 (x64/ARM64)",  "S5-08"),
        ("push",       "通知与后台同步",     "S5-09"),
    };

    public MainWindow()
    {
        InitializeComponent();
        Title = "CrossMail · 邮件";

        RootGrid.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        RootGrid.RowDefinitions.Add(new RowDefinition { Height = new GridLength(1, GridUnitType.Star) });
        RootGrid.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });

        var commandRow = BuildCommandBar();
        Grid.SetRow(commandRow, 0);
        RootGrid.Children.Add(commandRow);
        var nav = BuildNavigation();
        Grid.SetRow(nav, 1);
        RootGrid.Children.Add(nav);
        Grid.SetRow(_statusBar, 2);
        _statusBar.Margin = new Thickness(16, 6, 16, 8);
        RootGrid.Children.Add(_statusBar);

        // 两项 unpackaged-WinUI 崩溃陷阱（--window-smoke 实测 0xC0000005）：
        //   1. 构造函数里选中导航项会在窗口激活前触发页面挂载；
        //   2. Frame.Navigate 在未打包窗口会崩，改用 Frame.Content 挂载。
        RootGrid.Loaded += (_, _) =>
            RootGrid.DispatcherQueue.TryEnqueue(() =>
            {
                if (_nav.SelectedItem is null) _nav.SelectedItem = _nav.MenuItems[0];
            });

        AppState.Current.Changed += UpdateStatusBar;
        _ = LoadSnapshotAsync();
    }

    private readonly Grid _commandRow = new() { Padding = new Thickness(16, 10, 16, 6) };

    private Grid BuildCommandBar()
    {
        _commandRow.Children.Add(_search);
        foreach (var (tag, label) in new (string, string)[]
                 {
                     ("search", "搜索"),
                     ("compose", "撰写"),
                     ("sync", "同步"),
                     ("refresh", "刷新"),
                 })
        {
            var b = new Button { Content = label, Tag = tag, Margin = new Thickness(8, 0, 0, 0) };
            b.Click += OnCommand;
            _commandButtons.Add(b);
            _commandRow.Children.Add(b);
        }
        _shellHint.Margin = new Thickness(16, 0, 0, 0);
        _commandRow.Children.Add(_shellHint);
        return _commandRow;
    }

    private NavigationView BuildNavigation()
    {
        var nav = new NavigationView
        {
            IsBackButtonVisible = NavigationViewBackButtonVisible.Collapsed,
            PaneDisplayMode = NavigationViewPaneDisplayMode.Left,
            IsSettingsVisible = false,
            OpenPaneLength = 200,
            Content = _frame,
        };
        foreach (var (tag, title) in ShellPages)
            nav.MenuItems.Add(new NavigationViewItem { Content = title, Tag = tag });
        nav.SelectionChanged += OnSelectionChanged;
        _nav = nav;
        return nav;
    }

    private void OnSelectionChanged(NavigationView sender, NavigationViewSelectionChangedEventArgs args)
    {
        if (args.SelectedItem is not NavigationViewItem item || item.Tag is not string tag) return;
        _frame.Content = PageRouter.Create(tag);
        _moduleHint = tag switch
        {
            "mail" => "三栏工作区 · 数据来自本地缓存，同步后更新",
            "calendar" or "contacts" or "files" or "apps" => "占位模块 · 核心无对应 FFI 能力",
            "settings" => "设置聚合页 · 5 分段真调核心，写操作需勾选确认",
            "accounts" => "验收台页面 · 顶部命令栏仅对邮件工作区生效",
            "diagnostics" => "S5-02~S5-09 八页 · 36 个真实 handler",
            _ => "",
        };
        _shellHint.Text = _moduleHint;
        UpdateCommandState();
        UpdateStatusBar();
    }

    private void UpdateCommandState()
    {
        bool enabled = _frame.Content is IWorkspaceCommands;
        _search.IsEnabled = enabled;
        foreach (var b in _commandButtons) b.IsEnabled = enabled;
    }

    private async void OnCommand(object sender, RoutedEventArgs e)
    {
        if (sender is not Button b || b.Tag is not string cmd) return;
        LastCommandTask = ExecuteCommandAsync(cmd);
        await LastCommandTask;
    }

    // 命令的唯一执行路径：按钮 Click 走这里，自动化也等待同一个 Task，
    // 不另建一条「看起来一样」的假 handler。
    private async Task<string> ExecuteCommandAsync(string cmd)
    {
        _commandTrace.Add(cmd);
        if (_frame.Content is not IWorkspaceCommands target)
        {
            _shellHint.Text = "当前模块不支持该命令（非邮件工作区）";
            return "skipped:non-workspace";
        }
        try
        {
            switch (cmd)
            {
                case "search": await target.SearchAsync(_search.Text); break;
                case "compose": target.OpenComposeWindow(); break;
                case "sync": await target.SyncAsync(); break;
                case "refresh": await target.RefreshAsync(); break;
            }
            _shellHint.Text = "命令已下发：" + cmd + " · " + _moduleHint;
            return "ok:" + cmd;
        }
        catch (Exception ex)
        {
            _shellHint.Text = "命令失败（真实错误）：" + ex;
            return "error:" + ex.GetType().Name;
        }
    }

    private async Task LoadSnapshotAsync()
    {
        try
        {
            var snapshot = await CoreBridge.GetSnapshotAsync();
            AppState.Current.Snapshot = snapshot;
            AppState.Current.Notify();
        }
        catch (Exception ex)
        {
            _statusBar.Text = "core_status 读取失败（真实错误）：" + ex.Message;
        }
    }

    private void UpdateStatusBar()
    {
        var s = AppState.Current.Snapshot;
        string core = s is null
            ? "核心状态读取中"
            : "核心 " + s.Version + " · 协议 " + s.Protocols + " · 账号 " + s.AccountCount +
              " · 缓存 " + s.CacheUsageBytes + "/" + s.CacheQuotaBytes + " B";
        // 未读角标：只有 PushEventDto 真正到达后才显示数字；没收到推送时明确写「未收到」。
        string badge = AppState.Current.UnreadTotal is ulong unread
            ? " · 未读 " + unread + "（推送）"
            : " · 未读 —（未收到推送）";
        try { _statusBar.Text = core + badge + "  |  " + _moduleHint; }
        catch { /* window may be closing during smoke */ }

        // v1.7：新邮件应用内醒目提醒（顶部命令栏），8s 后自动恢复模块提示
        try
        {
            var alert = AppState.Current.MailAlert;
            _shellHint.Text = string.IsNullOrEmpty(alert) ? _moduleHint : alert;
            _shellHint.Opacity = string.IsNullOrEmpty(alert) ? 0.75 : 1.0;
        }
        catch { }
    }
}
