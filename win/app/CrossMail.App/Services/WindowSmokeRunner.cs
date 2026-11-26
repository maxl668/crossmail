using System;
using System.Collections.Generic;
using System.IO;
using System.Threading.Tasks;
using CrossMail.App.Pages;
using CrossMail.Bindings;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation.Peers;
using Microsoft.UI.Xaml.Automation.Provider;
using Microsoft.UI.Xaml.Controls;

namespace CrossMail.App.Services;

// Verifies the UNPACKAGED delivery shape: a real WinUI window is created, XAML is
// loaded, the product shell (command bar + module nav + three-column workspace +
// status bar) is populated and the initial page really mounted.
// Phase 2 additions: the four shell command buttons are invoked through the REAL
// UI event routing (automation peer Invoke -> Button.Click), the settings aggregate
// page executes its non-destructive actions, an independent compose window really
// auto-saves and restores a local draft, and a synthetic push event updates the
// unread badge. This is NOT a human-interaction check and the push event is NOT a
// real Rust subscription (subscribe_push needs a real account).
public static class WindowSmokeRunner
{
    public static int ExitCode { get; private set; }

    public static async Task RunAsync(MainWindow window)
    {
        var lines = new List<string>();
        try
        {
            // Let the dispatcher finish layout and the initial page mount.
            await Task.Delay(500);

            string title = window.Title;
            UIElement? content = window.Content;
            int navItems = window.Nav.MenuItems.Count;
            string? tag = (window.Nav.SelectedItem as NavigationViewItem)?.Tag as string;
            string? page = window.CurrentPageType?.FullName;
            double width = window.Bounds.Width;
            double height = window.Bounds.Height;
            int columns = window.WorkspaceColumns;
            int commands = window.CommandButtons;
            int diagnostics = window.DiagnosticPageCount;

            var tags = new List<string>();
            foreach (object item in window.Nav.MenuItems)
                if (item is NavigationViewItem nvi && nvi.Tag is string t) tags.Add(t);

            lines.Add("DATA_ROOT_ISOLATED=" + CoreBridge.UseIsolatedDataRoot);
            lines.Add("DATA_ROOT_NAME=" + Path.GetFileName(CoreBridge.DataRoot));
            lines.Add("WINDOW_TITLE=" + title);
            lines.Add("WINDOW_CONTENT=" + (content?.GetType().FullName ?? "null"));
            lines.Add($"WINDOW_BOUNDS={width}x{height}");
            lines.Add("NAV_ITEMS=" + navItems);
            lines.Add("NAV_TAGS=" + string.Join(",", tags));
            lines.Add("NAV_SELECTED_TAG=" + (tag ?? "null"));
            lines.Add("FRAME_PAGE=" + (page ?? "null"));
            lines.Add("SHELL_COMMAND_BUTTONS=" + commands);
            lines.Add("SHELL_WORKSPACE_COLUMNS=" + columns);
            lines.Add("SHELL_DIAGNOSTIC_PAGES=" + diagnostics);
            lines.Add("SHELL_STATUS=" + window.StatusText);

            // ---------------------------------------------------------------- 1) shell buttons
            var trace = new List<string>();
            int buttonsOk = 0;
            foreach (Button b in window.CommandButtonControls)
            {
                string cmd = b.Tag as string ?? "null";
                try
                {
                    var peer = new ButtonAutomationPeer(b);
                    ((IInvokeProvider)peer).Invoke();   // real Click event routing
                    Task? pending = window.LastCommandTask;
                    if (pending is not null)
                    {
                        Task done = await Task.WhenAny(pending, Task.Delay(TimeSpan.FromSeconds(10)));
                        if (done != pending) throw new TimeoutException("command did not finish in 10s");
                        await pending;
                    }
                    trace.Add(cmd + ":ok");
                    buttonsOk++;
                }
                catch (Exception ex)
                {
                    trace.Add(cmd + ":FAIL(" + ex.GetType().Name + ":" + ex.Message + ")");
                }
            }
            int composeWindowsOpened = window.OpenComposeCount;
            window.CloseComposeWindows();
            lines.Add("SHELL_BUTTON_TRACE=" + string.Join("|", trace));
            lines.Add($"SHELL_BUTTONS_OK={buttonsOk} compose_windows_opened={composeWindowsOpened} compose_windows_closed=true");
            lines.Add("SHELL_COMMAND_TRACE=" + string.Join(",", window.CommandTrace));
            lines.Add("SHELL_LAST_HINT=" + window.CommandHint);

            // ---------------------------------------------------------------- 2) settings page
            var settings = new SettingsWorkspacePage();
            await Task.Delay(150);   // constructor fires an initial core_status call
            int settingsOk = 0;
            foreach (var action in settings.SafeActions)
            {
                try
                {
                    Task t = action.Handler();
                    Task done = await Task.WhenAny(t, Task.Delay(TimeSpan.FromSeconds(10)));
                    if (done != t) throw new TimeoutException();
                    await t;
                    settingsOk++;
                }
                catch (Exception ex)
                {
                    lines.Add("SETTINGS_ACTION_FAIL " + action.Label + ": " + ex.GetType().Name);
                }
            }
            lines.Add($"SETTINGS_SECTIONS={settings.SectionCount} guarded_actions={settings.GuardedActionCount} " +
                      $"safe_actions={settings.SafeActions.Count} executed={settings.ExecutedActions} ok={settingsOk}");
            lines.Add("SETTINGS_LAST_OUTPUT=" + settings.LastOutput.Replace("\r", " ").Replace("\n", " | "));

            // ---------------------------------------------------------------- 3) compose window
            bool composeOk = false, autosaveOk = false, restoredOk = false, noAccountOk = false;
            bool sendGuardOk = false;
            string composeDetail = "not-run";
            CrossMail.App.ComposeWindow? cw1 = null, cw2 = null;
            try
            {
                cw1 = new CrossMail.App.ComposeWindow("compose-smoke.json");
                cw1.Activate();
                cw1.SetContentForSmoke("shell-smoke@example.invalid", "SHELL_SMOKE_SUBJECT", "auto-save body");
                autosaveOk = cw1.FlushAutosave() && File.Exists(cw1.AutosavePath) &&
                             File.ReadAllText(cw1.AutosavePath).Contains("SHELL_SMOKE_SUBJECT");
                string saveResult = await cw1.SaveToCoreAsync();
                noAccountOk = saveResult.Contains("尚无账号") || saveResult.Contains("save_draft ->") ||
                              saveResult.Contains("真实错误");

                // 发信前置校验：空收件人必须在碰核心之前就被拦下（确定性、零网络）。
                cw1.SetContentForSmoke("", "SHELL_SMOKE_NO_RECIPIENT", "no recipient body");
                string sendGuard = await cw1.SendAsync();
                sendGuardOk = sendGuard.Contains("请至少填写一个收件人");

                cw2 = new CrossMail.App.ComposeWindow("compose-smoke.json");
                cw2.Activate();
                restoredOk = cw2.SubjectText == "SHELL_SMOKE_SUBJECT" && cw2.ToText == "shell-smoke@example.invalid";

                composeOk = autosaveOk && restoredOk && noAccountOk && sendGuardOk &&
                            cw1.FieldCount == 3 && cw1.ActionButtons == 5 && cw1.HasSendButton;
                composeDetail = $"fields={cw1.FieldCount} buttons={cw1.ActionButtons} send_button={cw1.HasSendButton} " +
                                $"send_guard={sendGuardOk} autosave_writes={cw1.AutosaveWrites} file={cw1.AutosavePath}";
            }
            catch (Exception ex)
            {
                composeDetail = ex.GetType().Name + ": " + ex.Message;
            }
            finally
            {
                try { cw1?.CloseSafely(); } catch { }
                try { cw2?.CloseSafely(); } catch { }
            }
            lines.Add($"COMPOSE_WINDOW_OK compose_ok={composeOk} autosave={autosaveOk} restore={restoredOk} " +
                      $"save_path_guarded={noAccountOk} {composeDetail}");

            // ---------------------------------------------------------------- 4) push badge
            bool badgeOk = false;
            try
            {
                var evt = new PushEventDto
                {
                    Kind = PushEventKindDto.Synced,
                    Reason = "shell-smoke-synthetic",
                    Inserted = 3,
                    UnreadTotal = 7,
                    UnreadByMailbox = Array.Empty<MailboxUnreadDto>(),
                    Error = "",
                };
                var badge = new PushBadgeService(DispatcherQueue.GetForCurrentThread());
                badge.OnEvent(evt);
                await Task.Delay(150);
                badgeOk = window.StatusText.Contains("未读 7（推送）");
                lines.Add("PUSH_BADGE_OK=" + badgeOk + " simulated=true real_subscribe_push=false status=" + window.StatusText);
            }
            catch (Exception ex)
            {
                lines.Add("PUSH_BADGE_ERROR " + ex.GetType().Name + ": " + ex.Message);
            }

            lines.Add("SHELL_STATUS_AFTER=" + window.StatusText);

            bool ok = content is Grid
                      && navItems == 8
                      && tag == "mail"
                      && page == "CrossMail.App.Pages.MailWorkspacePage"
                      && commands == 4
                      && columns == 3
                      && diagnostics == 8
                      && buttonsOk == 4
                      && window.CommandTrace.Count == 4
                      && settings.SectionCount == 5
                      && settings.SafeActions.Count >= 8
                      && settingsOk == settings.SafeActions.Count
                      && composeOk
                      && badgeOk
                      && width > 0 && height > 0;

            string verdict = ok
                ? "WINDOW_SMOKE_OK window_created=true xaml_loaded=true navigation_done=true product_shell=true " +
                  $"workspace_columns=3 bounds_valid=true shell_commands={buttonsOk} settings_sections={settings.SectionCount} " +
                  "compose_window=true push_badge=simulated"
                : "WINDOW_SMOKE_FAIL live window state assertion failed";
            lines.Add(verdict);
            ExitCode = ok ? 0 : 1;
        }
        catch (Exception ex)
        {
            lines.Add("WINDOW_SMOKE_ERROR " + ex.GetType().Name + ": " + ex.Message);
            ExitCode = 1;
        }
        finally
        {
            string path = Path.Combine(AppContext.BaseDirectory, "app-window-smoke-result.txt");
            try { File.WriteAllLines(path, lines); } catch { /* result file is best-effort */ }
            foreach (string line in lines) Console.Out.WriteLine("[WINDOW-SMOKE] " + line);
            try { Application.Current?.Exit(); } catch { /* message loop already gone */ }
        }
    }
}
