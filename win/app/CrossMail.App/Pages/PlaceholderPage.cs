using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Navigation;

namespace CrossMail.App.Pages;

public sealed class PlaceholderPage : Page
{
    private readonly TextBlock _title = new() { FontSize = 22, Margin = new Thickness(0, 0, 0, 12) };
    private readonly TextBlock _body = new() { TextWrapping = TextWrapping.Wrap, FontSize = 14 };

    public PlaceholderPage()
    {
        var panel = new StackPanel { Margin = new Thickness(24), Spacing = 8 };
        panel.Children.Add(_title);
        panel.Children.Add(_body);
        Content = new ScrollViewer { Content = panel };
    }

    protected override void OnNavigatedTo(NavigationEventArgs e)
    {
        base.OnNavigatedTo(e);
        string tag = e.Parameter as string ?? "(none)";
        _title.Text = tag;
        _body.Text = "该页面尚未实现（Step3 工程骨架阶段）。Step4 逐项接入 CrossMail.Bindings 的真实返回值。";
    }
}
