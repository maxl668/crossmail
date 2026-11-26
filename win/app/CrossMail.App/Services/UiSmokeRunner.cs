using System;
using System.Collections.Generic;
using System.IO;
using System.Threading.Tasks;
using CrossMail.App.Pages;
using Microsoft.UI.Xaml.Controls;

namespace CrossMail.App.Services;

public static class UiSmokeRunner
{
    // Starts the real WinUI Application, instantiates all eight pages, then EXECUTES
    // each button's real handler and asserts the page state actually changed.
    // This is automation of button handlers, NOT a human click/keyboard test, and NOT
    // UI event-dispatch verification (the pages are not attached to a visible window).
    private static readonly string[] RequiresRealAccount = { "subscribe_push", "sync_account" };

    public static async Task<int> RunAsync()
    {
        var lines = new List<string>();
        lines.Add("DATA_ROOT_ISOLATED=" + CoreBridge.UseIsolatedDataRoot);
        lines.Add("DATA_ROOT_NAME=" + Path.GetFileName(CoreBridge.DataRoot));
        int total = 0, ok = 0, fail = 0, timeout = 0, skipped = 0;
        try
        {
            foreach (var (tag, title, sprint) in MainWindow.Pages)
            {
                Type pageType = PageRouter.Resolve(tag);
                if (pageType == typeof(PlaceholderPage))
                    throw new InvalidOperationException("route not implemented: " + tag);
                var page = (CorePage)Activator.CreateInstance(pageType)!;
                var c = page.CountControls();
                if (c.Buttons == 0)
                    throw new InvalidOperationException("page has no clickable action: " + tag);
                // Pages fire an initial async load in their constructor; let it settle
                // before taking a baseline so its mutations are not credited to a handler.
                await Task.Delay(120);

                int pageOk = 0, pageFail = 0, pageTimeout = 0, pageSkipped = 0;
                foreach (var action in page.Actions)
                {
                    total++;
                    if (Array.Exists(RequiresRealAccount, k => action.Label.Contains(k, StringComparison.OrdinalIgnoreCase)))
                    {
                        pageSkipped++;
                        lines.Add($"  SKIP {tag} / {action.Label} (needs a real account; callback vtable lifetime unverified)");
                        continue;
                    }

                    int before = page.StateVersion;
                    try
                    {
                        Task task = action.Handler();
                        Task done = await Task.WhenAny(task, Task.Delay(TimeSpan.FromSeconds(10)));
                        if (done != task)
                        {
                            pageTimeout++;
                            lines.Add($"  TIMEOUT {tag} / {action.Label}");
                            continue;
                        }
                        await task;
                        if (page.StateVersion <= before)
                        {
                            pageFail++;
                            lines.Add($"  NO_STATE_MUTATION {tag} / {action.Label}");
                        }
                        else pageOk++;
                    }
                    catch (Exception ex)
                    {
                        pageFail++;
                        lines.Add($"  HANDLER_ERROR {tag} / {action.Label}: {ex.GetType().Name}: {ex.Message}");
                    }
                }

                ok += pageOk; fail += pageFail; timeout += pageTimeout; skipped += pageSkipped;
                lines.Add($"{sprint} {tag,-10} -> {pageType.Name,-16} buttons={c.Buttons} inputs={c.Inputs} " +
                          $"texts={c.Texts} handlers={page.Actions.Count} ok={pageOk} fail={pageFail} " +
                          $"timeout={pageTimeout} skipped={pageSkipped} title={title}");
                lines.Add("  OUTPUT " + page.OutputPreview());
                Publish(lines); // incremental: keep real evidence even if a later page aborts
            }

            if (fail > 0 || timeout > 0)
                throw new InvalidOperationException($"handler failures={fail} timeouts={timeout}");
            lines.Add($"UI_SMOKE_OK pages={MainWindow.Pages.Length} actions={total} ok={ok} skipped={skipped}");
            Publish(lines);
            Dump(lines);
            return 0;
        }
        catch (Exception ex)
        {
            lines.Add("UI_SMOKE_FAIL " + ex);
            Publish(lines);
            Dump(lines);
            return 1;
        }
    }

    private static void Publish(List<string> lines)
    {
        try
        {
            File.WriteAllLines(Path.Combine(AppContext.BaseDirectory, "app-ui-smoke-result.txt"), lines);
        }
        catch (IOException) { }
    }

    private static void Dump(List<string> lines)
    {
        foreach (string line in lines) Console.WriteLine("[UI-SMOKE] " + line);
    }
}
