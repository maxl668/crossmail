using System;
using System.Collections.Generic;
using System.Threading.Tasks;
using CrossMail.App.Services;
using CrossMail.Bindings;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace CrossMail.App.Pages;

// 所有页面的公共骨架：统一把 UDL 同步阻塞调用丢到后台线程，UI 线程只负责渲染。
// 任一真实调用失败时显示完整异常，不吞错、不填假数据。
public abstract class CorePage : Page
{
    // A real button-to-handler binding, exposed so --ui-smoke can execute the actual
    // handler and assert that the page state really changed (not just construct controls).
    public sealed record PageAction(string Label, Func<Task> Handler);

    protected readonly StackPanel Root;
    protected readonly TextBlock Output;
    protected readonly TextBlock Status;
    private readonly List<PageAction> _actions = new();

    public IReadOnlyList<PageAction> Actions => _actions;

    // Monotonic counter: a handler may finish with the same visible text as before
    // (e.g. delete_account followed by a refresh on an empty DB). Counting state
    // mutations proves the handler actually ran, without relying on final text.
    public int StateVersion { get; private set; }

    protected CorePage(string title, string sprint)
    {
        Root = new StackPanel { Margin = new Thickness(24), Spacing = 10 };
        Root.Children.Add(new TextBlock { Text = title + "  ·  " + sprint, FontSize = 24 });
        Status = new TextBlock { Text = "就绪", Opacity = 0.75, TextWrapping = TextWrapping.Wrap };
        Root.Children.Add(Status);
        Output = new TextBlock
        {
            Text = "",
            TextWrapping = TextWrapping.Wrap,
            IsTextSelectionEnabled = true,
        };
        Root.Children.Add(Output);
        Content = new ScrollViewer { Content = Root };
    }

    protected static TextBox Field(string header, string placeholder) => new()
    {
        Header = header,
        PlaceholderText = placeholder,
        MinWidth = 240,
    };

    protected static StackPanel Row(params UIElement[] items)
    {
        var p = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        foreach (var i in items) p.Children.Add(i);
        return p;
    }

    protected Button Action(string label, Func<Task> handler)
    {
        _actions.Add(new PageAction(label, handler));
        var b = new Button { Content = label };
        b.Click += async (_, _) => await handler();
        return b;
    }

    protected void Show(string text) { Output.Text = text; StateVersion++; }

    // Status + Output snapshot used by the UI smoke runner to prove a handler ran.
    public string Snapshot() => Status.Text + "\u0000" + Output.Text;
    public string OutputPreview(int max = 120)
    {
        string s = Output.Text.Replace("\r", " ").Replace("\n", " | ");
        return s.Length <= max ? s : s.Substring(0, max) + "...";
    }

    // 供 --ui-smoke 统计页面真实控件树；不依赖 Loaded/模板应用，直接遍历已构造的 Content。
    public (int Buttons, int Inputs, int Texts) CountControls()
    {
        int b = 0, i = 0, t = 0;
        Walk(Root, ref b, ref i, ref t);
        return (b, i, t);
    }

    private static void Walk(UIElement element, ref int b, ref int i, ref int t)
    {
        if (element is Button) b++;
        else if (element is TextBox || element is ComboBox || element is CheckBox) i++;
        else if (element is TextBlock) t++;

        if (element is Panel panel)
            foreach (UIElement child in panel.Children) Walk(child, ref b, ref i, ref t);
        else if (element is ContentControl cc && cc.Content is UIElement childElement)
            Walk(childElement, ref b, ref i, ref t);
    }

    private void SetStatus(string text) { Status.Text = text; StateVersion++; }

    protected async Task<T?> Guard<T>(string label, Func<T> call)
    {
        SetStatus(label + " ...");
        try
        {
            T value = await CoreBridge.RunAsync(call);
            SetStatus(label + " · OK");
            return value;
        }
        catch (Exception ex)
        {
            SetStatus(label + " · FAIL");
            Show(ex.ToString());
            return default;
        }
    }

    protected async Task<T?> GuardAsync<T>(string label, Func<Task<T>> call)
    {
        SetStatus(label + " ...");
        try
        {
            T value = await call();
            SetStatus(label + " · OK");
            return value;
        }
        catch (Exception ex)
        {
            SetStatus(label + " · FAIL");
            Show(ex.ToString());
            return default;
        }
    }

    // engine 的创建与释放封闭在同一次后台调用内，避免跨线程持有 SafeHandle。
    protected Task<T?> WithEngine<T>(string label, Func<CrossMailEngine, T> call)
        => Guard(label, () =>
        {
            using var engine = CrossMailCore.OpenEngine(CoreBridge.CreateConfig());
            return call(engine);
        });
}
