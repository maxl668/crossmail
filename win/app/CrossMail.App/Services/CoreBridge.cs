using System;
using System.IO;
using System.Security.Cryptography;
using System.Text;
using System.Threading.Tasks;
using CrossMail.Bindings;

namespace CrossMail.App.Services;

public sealed class CoreSnapshot
{
    public string Version { get; init; } = "";
    public string Protocols { get; init; } = "";
    public int AccountCount { get; init; }
    public ulong CacheUsageBytes { get; init; }
    public ulong CacheQuotaBytes { get; init; }
    public string EngineType { get; init; } = "";
}

public static class CoreBridge
{
    /// <summary>
    /// 冒烟 / 验证链会真调 save_draft 等写操作。若继续指向真实账号库，
    /// 每次跑验证链都会往用户库里塞草稿（历轮已真实发生两次）。因此所有
    /// --smoke / --ui-smoke / --window-smoke 进程自动切到独立数据根。
    /// 显式环境变量 CROSSMAIL_DATA_ROOT 优先级最高，供特殊联调使用。
    /// </summary>
    public static bool UseIsolatedDataRoot { get; private set; }

    public static void EnableIsolation()
    {
        lock (MasterKeyLock) { _cachedMasterKey = null; }
        UseIsolatedDataRoot = true;
    }

    public static string DataRoot
    {
        get
        {
            string? overrideRoot = Environment.GetEnvironmentVariable("CROSSMAIL_DATA_ROOT");
            if (!string.IsNullOrWhiteSpace(overrideRoot)) return overrideRoot;
            string name = UseIsolatedDataRoot ? "CrossMail-Smoke" : "CrossMail";
            return Path.Combine(
                Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), name);
        }
    }

    public static string MasterKeyPath => Path.Combine(DataRoot, "master.key");

    private static readonly object MasterKeyLock = new();
    private static string? _cachedMasterKey;

    /// <summary>
    /// 修复 P0：Rust 引擎在 master_key_b64=null 时，每次 open_engine 都随机生成新的
    /// ChaCha20-Poly1305 主密钥（crates/ffi/src/engine.rs:495-501），导致刚写入的
    /// 凭证密文在下一个引擎实例里必然解不开（「凭证解密失败」）。Windows 侧每个方法
    /// 都会 OpenEngine，因此账号创建后立刻同步/推送也必定失败。
    /// 这里持久化一份 32 字节主密钥并注入所有 EngineConfig，保证跨调用、跨进程一致。
    /// 文件落在 %LOCALAPPDATA%\CrossMail\master.key，继承该目录的用户私有 ACL；
    /// 只存密钥本身，不含账号凭据，不写日志、不上云、不跨机复制。
    /// </summary>
    public static string MasterKeyB64 => GetOrCreateMasterKey();

    public static string MasterKeyFingerprint =>
        Convert.ToHexString(SHA256.HashData(Encoding.UTF8.GetBytes(MasterKeyB64)))[..8];

    private static string GetOrCreateMasterKey()
    {
        lock (MasterKeyLock)
        {
            if (_cachedMasterKey is not null) return _cachedMasterKey;
            Directory.CreateDirectory(DataRoot);
            if (File.Exists(MasterKeyPath))
            {
                string saved = File.ReadAllText(MasterKeyPath).Trim();
                byte[] parsed;
                try
                {
                    string padded = saved.Replace('-', '+').Replace('_', '/');
                    padded = padded.PadRight(padded.Length + (4 - padded.Length % 4) % 4, '=');
                    parsed = Convert.FromBase64String(padded);
                }
                catch (Exception ex)
                {
                    throw new InvalidOperationException("主密钥文件损坏：" + MasterKeyPath, ex);
                }
                if (parsed.Length != 32)
                    throw new InvalidOperationException("主密钥长度异常：" + MasterKeyPath);
                _cachedMasterKey = saved;
                return saved;
            }
            string encoded = Convert.ToBase64String(RandomNumberGenerator.GetBytes(32))
                .TrimEnd('=').Replace('+', '-').Replace('/', '_');
            File.WriteAllText(MasterKeyPath, encoded); // .NET Core 默认 UTF-8 无 BOM
            _cachedMasterKey = encoded;
            return encoded;
        }
    }

    public static EngineConfig CreateConfig()
    {
        string root = DataRoot;
        string cache = Path.Combine(root, "cache");
        Directory.CreateDirectory(cache);
        return new EngineConfig
        {
            DbPath = Path.Combine(root, "crossmail.db"),
            CacheDir = cache,
            CacheQuotaBytes = 64UL * 1024 * 1024,
            MasterKeyB64 = GetOrCreateMasterKey(),
        };
    }

    // UDL 全部是同步阻塞接口，UI 线程禁止直调；统一从这里转后台线程。
    public static Task<T> RunAsync<T>(Func<T> call) => Task.Run(call);

    public static Task<CoreSnapshot> GetSnapshotAsync() => RunAsync(() =>
    {
        var status = CrossMailCore.CoreStatus();
        using var engine = CrossMailCore.OpenEngine(CreateConfig());
        var cache = engine.CacheStatus();
        return new CoreSnapshot
        {
            Version = status.Version,
            Protocols = string.Join(",", status.SupportedProtocols),
            AccountCount = engine.ListAccounts().Count,
            CacheUsageBytes = cache.UsageBytes,
            CacheQuotaBytes = cache.QuotaBytes,
            EngineType = engine.GetType().FullName ?? "",
        };
    });
}
