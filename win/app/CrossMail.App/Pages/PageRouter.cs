using System;
using Microsoft.UI.Xaml.Controls;

namespace CrossMail.App.Pages;

public static class PageRouter
{
    // S5-02 ~ S5-09 八个验收页：保持原样，供 --ui-smoke 逐页实例化并真执行 handler。
    public static Type Resolve(string tag) => tag switch
    {
        "init" => typeof(InitPage),
        "accounts" => typeof(AccountsPage),
        "maillist" => typeof(MailListPage),
        "reader" => typeof(ReaderPage),
        "compose" => typeof(ComposePage),
        "settings" => typeof(SettingsPage),
        "packaging" => typeof(PackagingPage),
        "push" => typeof(PushPage),
        _ => typeof(PlaceholderPage),
    };

    // 产品化 Shell 的路由工厂。占位模块用工厂而不是无参构造，避免伪造数据。
    public static Page Create(string tag) => tag switch
    {
        "mail" => new MailWorkspacePage(),
        "calendar" => ModulePlaceholderPage.Calendar(),
        "contacts" => ModulePlaceholderPage.Contacts(),
        "files" => ModulePlaceholderPage.Files(),
        "apps" => ModulePlaceholderPage.Apps(),
        "accounts" => new AccountsPage(),
        // 产品设置走聚合页；S5-07 验收页 SettingsPage 仍在 Resolve() 里供 --ui-smoke 执行。
        "settings" => new SettingsWorkspacePage(),
        "diagnostics" => new DiagnosticsPage(),
        _ => (Page)Activator.CreateInstance(Resolve(tag))!,
    };
}
