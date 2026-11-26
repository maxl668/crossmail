using System;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace CrossMail.App.Pages;

// S5-02~S5-09 八个验收页不再与产品模块并列，收进「工程验收」二级分区。
// 页面本身保持零改动：--ui-smoke 仍逐页实例化并真执行 36 个按钮 handler。
public sealed class DiagnosticsPage : Page
{
    private readonly ContentControl _host = new();
    private readonly TextBlock _caption = new() { TextWrapping = TextWrapping.Wrap, Opacity = 0.75 };

    public int DiagnosticPages => MainWindow.Pages.Length;
    public string CurrentDiagnosticPage => _host.Content?.GetType().Name ?? "none";

    public DiagnosticsPage()
    {
        var root = new Grid { Margin = new Thickness(16, 12, 16, 12) };
        root.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(210) });
        root.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        root.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        root.RowDefinitions.Add(new RowDefinition { Height = new GridLength(1, GridUnitType.Star) });

        var header = new TextBlock { Text = "工程验收 / 诊断", FontSize = 22 };
        Grid.SetRow(header, 0);
        Grid.SetColumnSpan(header, 2);
        root.Children.Add(header);

        var left = new StackPanel { Margin = new Thickness(0, 12, 12, 0), Spacing = 6 };
        foreach (var (tag, title, sprint) in MainWindow.Pages)
        {
            var b = new Button { Content = sprint + "  " + title, HorizontalAlignment = HorizontalAlignment.Stretch, Tag = tag };
            b.Click += (_, _) => Show(tag);
            left.Children.Add(b);
        }
        left.Children.Add(_caption);
        Grid.SetRow(left, 1);
        Grid.SetColumn(left, 0);
        root.Children.Add(left);

        var scroll = new ScrollViewer { Margin = new Thickness(12, 12, 0, 0) };
        scroll.Content = _host;
        Grid.SetRow(scroll, 1);
        Grid.SetColumn(scroll, 1);
        root.Children.Add(scroll);

        Content = root;
        Show(MainWindow.Pages[0].Tag);
    }

    private void Show(string tag)
    {
        Type pageType = PageRouter.Resolve(tag);
        _host.Content = (Page)Activator.CreateInstance(pageType)!;
        _caption.Text = "当前：" + tag + " → " + pageType.Name + Environment.NewLine +
                        "这些页面是真实的 FFI 验收台，保留原有 36 个 handler 与断言口径。";
    }
}
