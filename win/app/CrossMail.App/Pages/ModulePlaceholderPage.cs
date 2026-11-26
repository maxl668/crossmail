using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace CrossMail.App.Pages;

// 网页版 Bulwark 有日历/联系人/文件/应用四大模块，但当前 Rust 核心
// 没有对应的 FFI 方法（Engine.g.cs 36 个方法里相关方法数为 0）。
// 这里如实占位，绝不伪造界面数据；落地前提写清楚，避免后续被误当成已交付。
public sealed class ModulePlaceholderPage : Page
{
    public ModulePlaceholderPage(string title, string webRoute, string[] missingCapabilities)
    {
        var panel = new StackPanel { Margin = new Thickness(28), Spacing = 12 };
        panel.Children.Add(new TextBlock { Text = title, FontSize = 24 });
        panel.Children.Add(new TextBlock
        {
            Text = "对应网页版 Bulwark 路由 " + webRoute + "。该模块在 Windows 版尚未实现。",
            TextWrapping = TextWrapping.Wrap,
            Opacity = 0.8,
        });
        panel.Children.Add(new TextBlock
        {
            Text = "原因：CrossMail 核心当前只暴露 36 个 FFI 方法，覆盖邮件、账号、缓存、日志、同步与推送；本模块相关方法数为 0。",
            TextWrapping = TextWrapping.Wrap,
        });
        panel.Children.Add(new TextBlock { Text = "缺失能力清单", FontSize = 16 });
        foreach (string c in missingCapabilities)
            panel.Children.Add(new TextBlock { Text = "· " + c, TextWrapping = TextWrapping.Wrap, Opacity = 0.8 });
        panel.Children.Add(new TextBlock
        {
            Text = "落地前提：先扩展 Rust 核心与 UDL，重新生成 C# 绑定并跑通 260 项断言；Windows 消费层才接入真实数据。",
            TextWrapping = TextWrapping.Wrap,
            Opacity = 0.75,
        });
        Content = new ScrollViewer { Content = panel };
    }

    public static Page Calendar() => new ModulePlaceholderPage("日历", "/calendar",
        new[] { "日历列表与事件查询", "事件创建/修改/删除", "日程共享与 ICS 订阅", "提醒与忙闲查询" });

    public static Page Contacts() => new ModulePlaceholderPage("联系人", "/contacts",
        new[] { "地址簿与联系人查询", "联系人增删改", "撰写时收件人自动补全", "联系人组" });

    public static Page Files() => new ModulePlaceholderPage("文件", "/files",
        new[] { "文件树与存储配额", "上传/下载/删除", "附件转存", "回收站" });

    public static Page Apps() => new ModulePlaceholderPage("应用", "/apps",
        new[] { "插件注册与启停", "OAuth 客户端管理", "设置同步", "Web push 配置" });
}
