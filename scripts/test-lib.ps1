<#
test-lib.ps1 — 検証スクリプト共通ライブラリ

run-gate.ps1 / verify-harness.ps1 / verify-manual.ps1 / verify-sp*.ps1 が
dot-source する。verify-sp1/3/4/5 に 4 通りコピーされていた共通機構
（昇格・vcvars 取り込み・DLL 解放・登録・teardown・セーフスポーン・サマリ）の正本。

契約（release-lib.ps1 と同じ規律）:
  - dot-source すると関数定義のみ行われる。プロンプト表示・プロセス起動・exit
    などの副作用は一切無い（関数を呼んだ時にだけ起きる）。
  - 失敗通知は throw（呼び出し側の trap/catch/Fail に任せる）。
  - pwsh 7 専用（#Requires）。Windows PowerShell 5.1 では動かない。
    ※唯一の例外は sandbox-gate-inner.ps1（Sandbox 内に pwsh が無いため 5.1）。

exit code 規約（全検証スクリプト共通）:
  0 = pass / 1 = fail / 2 = 実行不能・タイムアウト・エビデンス無し
  ゲート系ランナーは Resolve-GateExit で「子 exit 0 なのに解析行ゼロ／FAIL·ERROR 行あり」を
  この規約へ正規化する（子の exit 0 を鵜呑みにしない fail-closed。非0 の子 exit は維持）。
#>
#Requires -Version 7.3
. (Join-Path $PSScriptRoot 'zenzai-runtime-manifest.ps1')
. (Join-Path $PSScriptRoot 'version-lifetime.ps1')

# ----------------------------------------------------------------------------
# 表示ヘルパ（8 スクリプットに散在していた色付き出力の統一版）
# ----------------------------------------------------------------------------

function Write-Banner([string]$Text) {
    $line = '=' * 78
    Write-Host ''; Write-Host $line -ForegroundColor Cyan
    Write-Host "  $Text" -ForegroundColor Cyan
    Write-Host $line -ForegroundColor Cyan
}

function Write-Step([string]$Text) { Write-Host ">> $Text" -ForegroundColor Yellow }
function Write-Ok([string]$Text)   { Write-Host "   [OK] $Text" -ForegroundColor Green }
function Write-Warn([string]$Text) { Write-Host "   [WARN] $Text" -ForegroundColor Yellow }
function Write-FailMsg([string]$Text) { Write-Host "   [FAIL] $Text" -ForegroundColor Red }

function New-RunOwnedDirectory {
    <#並行実行同士で成果物を共有しない、GUID 所有の実行ディレクトリを作る。#>
    param([Parameter(Mandatory)][string]$Root)
    New-Item -ItemType Directory -Path $Root -Force -ErrorAction Stop | Out-Null
    $runId = [Guid]::NewGuid().ToString('N')
    $path = Join-Path $Root $runId
    $item = New-Item -ItemType Directory -Path $path -ErrorAction Stop
    if (-not $item.PSIsContainer) { throw "run-owned path is not a directory: $path" }
    return [pscustomobject]@{ RunId = $runId; Path = $item.FullName }
}

# ----------------------------------------------------------------------------
# 管理者昇格
# ----------------------------------------------------------------------------

$script:ValidationGateMutex = $null
$script:ValidationGateMutexName = 'Global\nospacekey-validation-gate-v1'
$script:ValidationGateJob = $null
$script:ValidationGateRecoveryJob = $null
$script:ValidationGateRecoveryJobName = $null
$script:ValidationGateRecoveryJobSddl = $null
$script:ValidationGateRecoveryLease = $null
$script:ValidationGateAdminRecoveryJobName = `
    'Global\nospacekey-validation-gate-recovery-admin-v1'
$script:ValidationGateUserRecoveryJobName = `
    'Global\nospacekey-validation-gate-recovery-user-v1'
$script:ValidationGateLeaseAccepted = $false
$script:ValidationRegistrationHelperUnconfirmed = $false
$script:ValidationCleanupDirectory = $null
$script:RegisteredTipSnapshot = $null
$script:AllowInsecureCleanupDirectoryForTesting = $false
$script:NospacekeyClsid = '{B4B39227-EFF2-41DA-B357-0C3170A57875}'
$script:ValidationCleanupDirectorySddl = 'O:BAG:BAD:P' +
    '(A;OICI;FA;;;BA)(A;OICI;FA;;;SY)' +
    '(A;OICI;0x1200a9;;;BU)' +
    '(A;OICI;0x1200a9;;;S-1-15-2-1)' +
    '(A;OICI;0x1200a9;;;S-1-15-2-2)'
$script:ValidationRepoRoot = [IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
$script:ValidationTipDll = [IO.Path]::GetFullPath(
    (Join-Path $script:ValidationRepoRoot 'target\release\nospacekey_tip.dll'))
$script:ValidationSwiftResourceNames = @(
    'AzooKeyKanaKanjiConverter_KanaKanjiConverterModuleWithDefaultDictionary.resources',
    'AzooKeyKanaKanjiConverter_EfficientNGram.resources',
    'swift-transformers_Hub.resources'
)
$script:ValidationRequiredSwiftDllNames = @(
    'swiftCore.dll',
    'swift_Concurrency.dll',
    'swift_StringProcessing.dll',
    'Foundation.dll',
    'FoundationEssentials.dll',
    'FoundationNetworking.dll',
    'swiftWinSDK.dll',
    'swiftCRT.dll',
    'swiftDispatch.dll',
    'BlocksRuntime.dll'
)
$script:ValidationCanonicalVcDllNames = @(
    'vcomp140.dll',
    'vcruntime140.dll',
    'vcruntime140_1.dll',
    'msvcp140.dll'
)

function Initialize-ValidationGateNative {
    if ('NospacekeyValidationNative' -as [type]) { return }
    Add-Type -TypeDefinition @"
using System;
using System.ComponentModel;
using System.Diagnostics;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using Microsoft.Win32.SafeHandles;

public static class NospacekeyValidationNative
{
    private const uint TH32CS_SNAPPROCESS = 0x00000002;
    private static readonly IntPtr INVALID_HANDLE_VALUE = new IntPtr(-1);

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    private struct PROCESSENTRY32
    {
        public uint dwSize;
        public uint cntUsage;
        public uint th32ProcessID;
        public IntPtr th32DefaultHeapID;
        public uint th32ModuleID;
        public uint cntThreads;
        public uint th32ParentProcessID;
        public int pcPriClassBase;
        public uint dwFlags;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 260)]
        public string szExeFile;
    }

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern IntPtr CreateToolhelp32Snapshot(uint flags, uint processId);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern bool Process32FirstW(IntPtr snapshot, ref PROCESSENTRY32 entry);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern bool Process32NextW(IntPtr snapshot, ref PROCESSENTRY32 entry);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool CloseHandle(IntPtr handle);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern IntPtr OpenJobObjectW(uint desiredAccess, bool inheritHandle, string name);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool IsProcessInJob(IntPtr process, IntPtr job, out bool result);
    [DllImport("kernel32.dll")]
    private static extern IntPtr GetCurrentProcess();
    [DllImport("advapi32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern bool ConvertStringSecurityDescriptorToSecurityDescriptorW(
        string descriptor, uint revision, out IntPtr securityDescriptor, out uint size);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern bool CreateDirectoryW(string path, ref SECURITY_ATTRIBUTES attributes);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern SafeFileHandle CreateFileW(
        string fileName, uint desiredAccess, uint shareMode, IntPtr securityAttributes,
        uint creationDisposition, uint flagsAndAttributes, IntPtr templateFile);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool GetFileInformationByHandleEx(
        SafeFileHandle file, int fileInformationClass,
        out FILE_ATTRIBUTE_TAG_INFO information, uint size);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern uint GetFinalPathNameByHandleW(
        SafeFileHandle file, StringBuilder path, uint pathLength, uint flags);
    [DllImport("kernel32.dll")]
    private static extern IntPtr LocalFree(IntPtr memory);

    private const uint JOB_OBJECT_QUERY = 0x0004;
    private const uint GENERIC_READ = 0x80000000;
    private const uint FILE_READ_ATTRIBUTES = 0x00000080;
    private const uint FILE_SHARE_READ = 0x00000001;
    private const uint FILE_SHARE_WRITE = 0x00000002;
    private const uint FILE_SHARE_DELETE = 0x00000004;
    private const uint OPEN_EXISTING = 3;
    private const uint FILE_FLAG_OPEN_REPARSE_POINT = 0x00200000;
    private const uint FILE_FLAG_BACKUP_SEMANTICS = 0x02000000;
    private const uint FILE_ATTRIBUTE_DIRECTORY = 0x00000010;
    private const uint FILE_ATTRIBUTE_REPARSE_POINT = 0x00000400;
    private const int FileAttributeTagInfo = 9;

    [StructLayout(LayoutKind.Sequential)]
    private struct SECURITY_ATTRIBUTES
    {
        public uint nLength;
        public IntPtr lpSecurityDescriptor;
        [MarshalAs(UnmanagedType.Bool)] public bool bInheritHandle;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct FILE_ATTRIBUTE_TAG_INFO
    {
        public uint FileAttributes;
        public uint ReparseTag;
    }

    public static int GetParentProcessId(int processId)
    {
        IntPtr snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if (snapshot == INVALID_HANDLE_VALUE) throw new Win32Exception();
        try
        {
            var entry = new PROCESSENTRY32();
            entry.dwSize = (uint)Marshal.SizeOf(typeof(PROCESSENTRY32));
            if (!Process32FirstW(snapshot, ref entry)) throw new Win32Exception();
            do
            {
                if (entry.th32ProcessID == (uint)processId)
                    return checked((int)entry.th32ParentProcessID);
            } while (Process32NextW(snapshot, ref entry));
            throw new InvalidOperationException("current process missing from process snapshot");
        }
        finally { CloseHandle(snapshot); }
    }

    public static SafeWaitHandle OpenCurrentProcessJobLease(string name)
    {
        IntPtr job = OpenJobObjectW(JOB_OBJECT_QUERY, false, name);
        if (job == IntPtr.Zero) throw new Win32Exception();
        try
        {
            bool result;
            if (!IsProcessInJob(GetCurrentProcess(), job, out result))
                throw new Win32Exception();
            if (!result)
                throw new InvalidOperationException("current process is outside recovery job");
            var lease = new SafeWaitHandle(job, true);
            job = IntPtr.Zero;
            return lease;
        }
        finally { if (job != IntPtr.Zero) CloseHandle(job); }
    }

    public static void CreateSecureDirectory(string path, string sddl)
    {
        // Passing the complete descriptor to CreateDirectoryW makes creation
        // and the trust-boundary ACL one atomic operation.
        IntPtr descriptor;
        uint descriptorSize;
        if (!ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl, 1, out descriptor, out descriptorSize))
            throw new Win32Exception();
        try
        {
            var attributes = new SECURITY_ATTRIBUTES();
            attributes.nLength = (uint)Marshal.SizeOf(typeof(SECURITY_ATTRIBUTES));
            attributes.lpSecurityDescriptor = descriptor;
            attributes.bInheritHandle = false;
            if (!CreateDirectoryW(path, ref attributes)) throw new Win32Exception();
        }
        finally { LocalFree(descriptor); }
    }

    private static string NormalizeFinalPath(string path)
    {
        const string uncPrefix = @"\\?\UNC\";
        const string localPrefix = @"\\?\";
        if (path.StartsWith(uncPrefix, StringComparison.OrdinalIgnoreCase))
            return @"\\" + path.Substring(uncPrefix.Length);
        if (path.StartsWith(localPrefix, StringComparison.OrdinalIgnoreCase))
            return path.Substring(localPrefix.Length);
        return path;
    }

    private static string GetNormalizedFinalPath(SafeFileHandle handle)
    {
        var finalPath = new StringBuilder(512);
        uint required = GetFinalPathNameByHandleW(
            handle, finalPath, (uint)finalPath.Capacity, 0);
        if (required == 0) throw new Win32Exception();
        if (required >= finalPath.Capacity)
        {
            finalPath.Capacity = checked((int)required + 1);
            required = GetFinalPathNameByHandleW(
                handle, finalPath, (uint)finalPath.Capacity, 0);
            if (required == 0 || required >= finalPath.Capacity)
                throw new Win32Exception();
        }
        return Path.GetFullPath(NormalizeFinalPath(finalPath.ToString()));
    }

    public static FileStream OpenPinnedReadFile(string path, string trustedRoot)
    {
        string fullPath = Path.GetFullPath(path);
        string rootPath = Path.GetFullPath(trustedRoot).TrimEnd('\\');
        string relativePath = Path.GetRelativePath(rootPath, fullPath);
        if (Path.IsPathRooted(relativePath) || relativePath == ".." ||
            relativePath.StartsWith(".." + Path.DirectorySeparatorChar,
                StringComparison.Ordinal))
            throw new IOException("pinned validation input is outside its trusted root");

        using (SafeFileHandle root = CreateFileW(
            rootPath, FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            IntPtr.Zero, OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            IntPtr.Zero))
        {
            if (root.IsInvalid) throw new Win32Exception();
            FILE_ATTRIBUTE_TAG_INFO rootAttributes;
            if (!GetFileInformationByHandleEx(
                root, FileAttributeTagInfo, out rootAttributes,
                (uint)Marshal.SizeOf(typeof(FILE_ATTRIBUTE_TAG_INFO))))
                throw new Win32Exception();
            if ((rootAttributes.FileAttributes & FILE_ATTRIBUTE_DIRECTORY) == 0 ||
                (rootAttributes.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT) != 0)
                throw new IOException("validation trusted root is not a regular directory");

            string resolvedRoot = GetNormalizedFinalPath(root).TrimEnd('\\');
            string expectedFinalPath = Path.GetFullPath(
                Path.Combine(resolvedRoot, relativePath));

        SafeFileHandle handle = CreateFileW(
            fullPath, GENERIC_READ, FILE_SHARE_READ, IntPtr.Zero, OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT, IntPtr.Zero);
        if (handle.IsInvalid)
        {
            int error = Marshal.GetLastWin32Error();
            handle.Dispose();
            throw new Win32Exception(error);
        }
        try
        {
            FILE_ATTRIBUTE_TAG_INFO attributes;
            if (!GetFileInformationByHandleEx(
                handle, FileAttributeTagInfo, out attributes,
                (uint)Marshal.SizeOf(typeof(FILE_ATTRIBUTE_TAG_INFO))))
                throw new Win32Exception();
            if ((attributes.FileAttributes &
                (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT)) != 0)
                throw new IOException("pinned validation input is not a regular file");

            string resolved = GetNormalizedFinalPath(handle);
            if (!resolved.Equals(expectedFinalPath, StringComparison.OrdinalIgnoreCase))
                throw new IOException("pinned validation input resolved outside its exact path");

            var stream = new FileStream(handle, FileAccess.Read, 4096, false);
            handle = null;
            return stream;
        }
        finally
        {
            if (handle != null) handle.Dispose();
        }
        }
    }
}

public sealed class NospacekeyKillOnCloseJob : IDisposable
{
    private const uint JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE = 0x00002000;
    private const int JobObjectExtendedLimitInformation = 9;
    private const int JobObjectBasicAccountingInformation = 1;
    private const uint CREATE_SUSPENDED = 0x00000004;
    private const uint CREATE_UNICODE_ENVIRONMENT = 0x00000400;
    private const int ERROR_ALREADY_EXISTS = 183;
    private const int ERROR_FILE_NOT_FOUND = 2;
    private const int ERROR_INVALID_NAME = 123;
    private const uint JOB_OBJECT_QUERY = 0x0004;
    private IntPtr handle;

    [StructLayout(LayoutKind.Sequential)]
    private struct JOBOBJECT_BASIC_LIMIT_INFORMATION
    {
        public long PerProcessUserTimeLimit;
        public long PerJobUserTimeLimit;
        public uint LimitFlags;
        public UIntPtr MinimumWorkingSetSize;
        public UIntPtr MaximumWorkingSetSize;
        public uint ActiveProcessLimit;
        public UIntPtr Affinity;
        public uint PriorityClass;
        public uint SchedulingClass;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct IO_COUNTERS
    {
        public ulong ReadOperationCount;
        public ulong WriteOperationCount;
        public ulong OtherOperationCount;
        public ulong ReadTransferCount;
        public ulong WriteTransferCount;
        public ulong OtherTransferCount;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct JOBOBJECT_EXTENDED_LIMIT_INFORMATION
    {
        public JOBOBJECT_BASIC_LIMIT_INFORMATION BasicLimitInformation;
        public IO_COUNTERS IoInfo;
        public UIntPtr ProcessMemoryLimit;
        public UIntPtr JobMemoryLimit;
        public UIntPtr PeakProcessMemoryUsed;
        public UIntPtr PeakJobMemoryUsed;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct JOBOBJECT_BASIC_ACCOUNTING_INFORMATION
    {
        public long TotalUserTime;
        public long TotalKernelTime;
        public long ThisPeriodTotalUserTime;
        public long ThisPeriodTotalKernelTime;
        public uint TotalPageFaultCount;
        public uint TotalProcesses;
        public uint ActiveProcesses;
        public uint TotalTerminatedProcesses;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct SECURITY_ATTRIBUTES
    {
        public uint nLength;
        public IntPtr lpSecurityDescriptor;
        [MarshalAs(UnmanagedType.Bool)] public bool bInheritHandle;
    }

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    private struct STARTUPINFO
    {
        public uint cb;
        public IntPtr lpReserved;
        public IntPtr lpDesktop;
        public IntPtr lpTitle;
        public uint dwX;
        public uint dwY;
        public uint dwXSize;
        public uint dwYSize;
        public uint dwXCountChars;
        public uint dwYCountChars;
        public uint dwFillAttribute;
        public uint dwFlags;
        public ushort wShowWindow;
        public ushort cbReserved2;
        public IntPtr lpReserved2;
        public IntPtr hStdInput;
        public IntPtr hStdOutput;
        public IntPtr hStdError;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct PROCESS_INFORMATION
    {
        public IntPtr hProcess;
        public IntPtr hThread;
        public uint dwProcessId;
        public uint dwThreadId;
    }

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern IntPtr CreateJobObjectW(IntPtr attributes, string name);
    [DllImport("kernel32.dll", EntryPoint = "CreateJobObjectW",
        CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern IntPtr CreateJobObjectWithSecurityW(
        ref SECURITY_ATTRIBUTES attributes, string name);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern IntPtr OpenJobObjectW(
        uint desiredAccess, bool inheritHandle, string name);
    [DllImport("advapi32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern bool ConvertStringSecurityDescriptorToSecurityDescriptorW(
        string descriptor, uint revision, out IntPtr securityDescriptor, out uint size);
    [DllImport("kernel32.dll")]
    private static extern IntPtr LocalFree(IntPtr memory);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool SetInformationJobObject(
        IntPtr job, int infoClass, ref JOBOBJECT_EXTENDED_LIMIT_INFORMATION info, uint length);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool AssignProcessToJobObject(IntPtr job, IntPtr process);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool TerminateJobObject(IntPtr job, uint exitCode);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool QueryInformationJobObject(
        IntPtr job, int infoClass, out JOBOBJECT_BASIC_ACCOUNTING_INFORMATION info,
        uint length, IntPtr returnLength);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern bool CreateProcessW(
        string applicationName, StringBuilder commandLine,
        IntPtr processAttributes, IntPtr threadAttributes, bool inheritHandles,
        uint creationFlags, IntPtr environment, string currentDirectory,
        ref STARTUPINFO startupInfo, out PROCESS_INFORMATION processInformation);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern uint ResumeThread(IntPtr thread);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool TerminateProcess(IntPtr process, uint exitCode);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool CloseHandle(IntPtr handle);

    public NospacekeyKillOnCloseJob()
    {
        handle = CreateJobObjectW(IntPtr.Zero, null);
        if (handle == IntPtr.Zero) throw new Win32Exception();
        ConfigureKillOnClose();
    }

    public NospacekeyKillOnCloseJob(string name, string sddl)
    {
        if (String.IsNullOrWhiteSpace(name))
            throw new ArgumentException("job name is required", "name");
        if (String.IsNullOrWhiteSpace(sddl))
            throw new ArgumentException("job security descriptor is required", "sddl");
        IntPtr descriptor;
        uint descriptorSize;
        if (!ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl, 1, out descriptor, out descriptorSize))
            throw new Win32Exception();
        try
        {
            var attributes = new SECURITY_ATTRIBUTES();
            attributes.nLength = (uint)Marshal.SizeOf(typeof(SECURITY_ATTRIBUTES));
            attributes.lpSecurityDescriptor = descriptor;
            attributes.bInheritHandle = false;
            handle = CreateJobObjectWithSecurityW(ref attributes, name);
            int error = Marshal.GetLastWin32Error();
            if (handle == IntPtr.Zero) throw new Win32Exception(error);
            if (error == ERROR_ALREADY_EXISTS)
            {
                CloseHandle(handle);
                handle = IntPtr.Zero;
                throw new InvalidOperationException("named recovery job unexpectedly exists");
            }
        }
        finally { LocalFree(descriptor); }
        ConfigureKillOnClose();
    }

    private void ConfigureKillOnClose()
    {
        var info = new JOBOBJECT_EXTENDED_LIMIT_INFORMATION();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if (!SetInformationJobObject(
            handle, JobObjectExtendedLimitInformation, ref info,
            (uint)Marshal.SizeOf(typeof(JOBOBJECT_EXTENDED_LIMIT_INFORMATION))))
        {
            int error = Marshal.GetLastWin32Error();
            CloseHandle(handle);
            handle = IntPtr.Zero;
            throw new Win32Exception(error);
        }
    }

    public void AddProcess(Process process)
    {
        if (handle == IntPtr.Zero) throw new ObjectDisposedException(GetType().Name);
        if (!AssignProcessToJobObject(handle, process.Handle)) throw new Win32Exception();
    }

    private static string QuoteArgument(string value)
    {
        if (value == null) throw new ArgumentNullException("value");
        if (value.Length > 0 && value.IndexOfAny(new[] { ' ', '\t', '\n', '\v', '"' }) < 0)
            return value;
        var result = new StringBuilder();
        result.Append('"');
        int slashes = 0;
        foreach (char c in value)
        {
            if (c == '\\') { slashes++; continue; }
            if (c == '"')
            {
                result.Append('\\', slashes * 2 + 1);
                result.Append('"');
                slashes = 0;
                continue;
            }
            result.Append('\\', slashes);
            slashes = 0;
            result.Append(c);
        }
        result.Append('\\', slashes * 2);
        result.Append('"');
        return result.ToString();
    }

    public Process StartProcess(string filePath, string[] arguments)
    {
        return StartProcess(filePath, arguments, null);
    }

    public Process StartProcess(
        string filePath, string[] arguments, NospacekeyKillOnCloseJob additionalJob)
    {
        if (handle == IntPtr.Zero) throw new ObjectDisposedException(GetType().Name);
        if (additionalJob != null && additionalJob.handle == IntPtr.Zero)
            throw new ObjectDisposedException(additionalJob.GetType().Name);
        string fullPath = Path.GetFullPath(filePath);
        if (!Path.IsPathRooted(filePath) || !File.Exists(fullPath))
            throw new FileNotFoundException("job-bound executable was not found", fullPath);

        var commandLine = new StringBuilder(QuoteArgument(fullPath));
        if (arguments != null)
        {
            foreach (string argument in arguments)
            {
                commandLine.Append(' ');
                commandLine.Append(QuoteArgument(argument));
            }
        }

        var startup = new STARTUPINFO();
        startup.cb = (uint)Marshal.SizeOf(typeof(STARTUPINFO));
        PROCESS_INFORMATION native = new PROCESS_INFORMATION();
        Process managed = null;
        bool created = false;
        bool resumed = false;
        try
        {
            if (!CreateProcessW(fullPath, commandLine, IntPtr.Zero, IntPtr.Zero, false,
                CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT, IntPtr.Zero,
                Directory.GetCurrentDirectory(), ref startup, out native))
                throw new Win32Exception();
            created = true;
            if (!AssignProcessToJobObject(handle, native.hProcess))
                throw new Win32Exception();
            if (additionalJob != null &&
                !AssignProcessToJobObject(additionalJob.handle, native.hProcess))
                throw new Win32Exception();
            managed = Process.GetProcessById(checked((int)native.dwProcessId));
            // Process.GetProcessById is PID-backed until its Handle is first
            // opened. If the child exits after the native CreateProcess handle
            // is closed but before that lazy open, Process.ExitCode becomes
            // unavailable (PowerShell observes $null and can turn failure into
            // exit 0). Force/cache the managed handle while still suspended.
            if (managed.Handle == IntPtr.Zero)
                throw new InvalidOperationException("failed to open managed process handle");
            if (ResumeThread(native.hThread) == 0xFFFFFFFF) throw new Win32Exception();
            resumed = true;
            return managed;
        }
        finally
        {
            if (created && !resumed)
            {
                TerminateProcess(native.hProcess, 2);
                if (managed != null) managed.Dispose();
            }
            if (native.hThread != IntPtr.Zero) CloseHandle(native.hThread);
            if (native.hProcess != IntPtr.Zero) CloseHandle(native.hProcess);
        }
    }

    private static bool TerminateAndWaitHandle(IntPtr job, int timeoutMs)
    {
        if (!TerminateJobObject(job, 2)) throw new Win32Exception();
        var stopwatch = Stopwatch.StartNew();
        while (true)
        {
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION info;
            if (!QueryInformationJobObject(
                job, JobObjectBasicAccountingInformation, out info,
                (uint)Marshal.SizeOf(typeof(JOBOBJECT_BASIC_ACCOUNTING_INFORMATION)),
                IntPtr.Zero))
                throw new Win32Exception();
            if (info.ActiveProcesses == 0) return true;
            if (stopwatch.ElapsedMilliseconds >= timeoutMs) return false;
            Thread.Sleep(25);
        }
    }

    public bool TerminateAndWait(int timeoutMs)
    {
        if (handle == IntPtr.Zero) throw new ObjectDisposedException(GetType().Name);
        return TerminateAndWaitHandle(handle, timeoutMs);
    }

    public static bool WaitForNamedJobToDisappear(string name, int timeoutMs)
    {
        var stopwatch = Stopwatch.StartNew();
        while (true)
        {
            IntPtr existing = OpenJobObjectW(JOB_OBJECT_QUERY, false, name);
            if (existing == IntPtr.Zero)
            {
                int error = Marshal.GetLastWin32Error();
                if (error == ERROR_FILE_NOT_FOUND || error == ERROR_INVALID_NAME) return true;
                throw new Win32Exception(error);
            }
            CloseHandle(existing);
            if (stopwatch.ElapsedMilliseconds >= timeoutMs) return false;
            Thread.Sleep(25);
        }
    }

    public void Dispose()
    {
        if (handle == IntPtr.Zero) return;
        CloseHandle(handle);
        handle = IntPtr.Zero;
        GC.SuppressFinalize(this);
    }

    ~NospacekeyKillOnCloseJob() { Dispose(); }
}
"@
}

function Test-SupervisedValidationGateLease {
    $leaseId = [string]$env:NOSPACEKEY_VALIDATION_GATE_LEASE_ID
    $supervisorPidText = [string]$env:NOSPACEKEY_VALIDATION_GATE_SUPERVISOR_PID
    $scope = [string]$env:NOSPACEKEY_VALIDATION_GATE_SCOPE
    if ($leaseId -notmatch '^[0-9a-f]{32}$' -or $supervisorPidText -notmatch '^\d+$' -or
        $scope -notin @('Admin', 'User')) {
        return $false
    }
    $recoveryJobName = if ($scope -eq 'Admin') {
        $script:ValidationGateAdminRecoveryJobName
    } else {
        $script:ValidationGateUserRecoveryJobName
    }
    $recoveryLease = $null
    try {
        Initialize-ValidationGateNative
        $supervisorPid = [int]$supervisorPidText
        if ([NospacekeyValidationNative]::GetParentProcessId($PID) -ne $supervisorPid) {
            return $false
        }
        $supervisor = [Diagnostics.Process]::GetProcessById($supervisorPid)
        if ($supervisor.HasExited) { return $false }

        # The target must have been assigned while suspended to the fixed
        # recovery Job for this trust scope. Keep the query-only handle alive:
        # after a supervisor crash, the name cannot disappear until this target
        # has also exited, which gives the next owner an exact drain barrier.
        $recoveryLease = [NospacekeyValidationNative]::OpenCurrentProcessJobLease(
            $recoveryJobName)

        $probe = [Threading.Mutex]::new($false, $script:ValidationGateMutexName)
        $probeAcquired = $false
        try {
            try { $probeAcquired = $probe.WaitOne(0) }
            catch [Threading.AbandonedMutexException] { $probeAcquired = $true }
            if ($probeAcquired) {
                $probe.ReleaseMutex()
                return $false
            }
        } finally {
            $probe.Dispose()
        }
        if ($supervisor.HasExited) { return $false }
        $script:ValidationGateRecoveryLease = $recoveryLease
        $recoveryLease = $null
        return $true
    } catch {
        return $false
    } finally {
        if ($null -ne $recoveryLease) { $recoveryLease.Dispose() }
    }
}

function Enter-ValidationGateLock {
    <#
    Serialize host-side validation that mutates the process-global engine or
    machine-wide TIP registration. A supervised elevated target may skip a
    second acquire only after validating its real parent, fixed recovery Job
    membership, and the supervisor-held mutex. Raw environment state is not proof.
    #>
    param(
        [int]$WaitMs = 0,
        [switch]$RequireOwnership,
        [ValidateSet('Admin', 'User')][string]$RecoveryScope = 'User'
    )
    if ($null -ne $script:ValidationGateMutex) { return $true }

    $hasLeaseClaim = -not [string]::IsNullOrWhiteSpace(
        [string]$env:NOSPACEKEY_VALIDATION_GATE_LEASE_ID) -or
        -not [string]::IsNullOrWhiteSpace(
            [string]$env:NOSPACEKEY_VALIDATION_GATE_SUPERVISOR_PID) -or
        -not [string]::IsNullOrWhiteSpace(
            [string]$env:NOSPACEKEY_VALIDATION_GATE_SCOPE)
    if (-not $RequireOwnership -and $hasLeaseClaim) {
        # A malformed/stale claim is fail-closed. Falling back to an abandoned
        # mutex could let an orphan target outlive its supervisor.
        $script:ValidationGateLeaseAccepted = Test-SupervisedValidationGateLease
        return $script:ValidationGateLeaseAccepted
    }

    $mutex = [Threading.Mutex]::new($false, $script:ValidationGateMutexName)
    $acquired = $false
    try {
        $acquired = $mutex.WaitOne($WaitMs)
    } catch [Threading.AbandonedMutexException] {
        # WaitOne transfers ownership when it reports abandonment. The fixed
        # recovery Job below remains named until the old target tree has closed
        # its last query lease, so recovery can be completed in this same run.
        $acquired = $true
    } catch {
        $mutex.Dispose()
        throw
    }
    if (-not $acquired) {
        $mutex.Dispose()
        return $false
    }
    try {
        Initialize-ValidationGateNative
        $recoveryName = if ($RecoveryScope -eq 'Admin') {
            $script:ValidationGateAdminRecoveryJobName
        } else {
            $script:ValidationGateUserRecoveryJobName
        }
        foreach ($priorName in @(
                $script:ValidationGateAdminRecoveryJobName,
                $script:ValidationGateUserRecoveryJobName)) {
            if (-not [NospacekeyKillOnCloseJob]::WaitForNamedJobToDisappear(
                    $priorName, 10000)) {
                throw "previous validation recovery job did not drain; mutex remains held fail-closed: $priorName"
            }
        }
        $currentSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
        $recoverySddl = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x0004;;;$currentSid)"
        $recoveryJob = [NospacekeyKillOnCloseJob]::new($recoveryName, $recoverySddl)
        try {
            # Only this unnamed handle controls target lifetime. It cannot be
            # reopened by name, so a peer cannot suppress KILL_ON_JOB_CLOSE.
            $job = [NospacekeyKillOnCloseJob]::new()
        } catch {
            $recoveryJob.Dispose()
            throw
        }
        $script:ValidationGateJob = $job
        $script:ValidationGateRecoveryJob = $recoveryJob
        $script:ValidationGateRecoveryJobName = $recoveryName
        $script:ValidationGateRecoveryJobSddl = $recoverySddl
        $script:ValidationGateMutex = $mutex
        $script:ValidationGateLeaseAccepted = $false
        return $true
    } catch {
        try { $mutex.ReleaseMutex() } catch { }
        $mutex.Dispose()
        throw
    }
}

function Reset-ValidationGateJob {
    <#
    Terminate and positively drain every process created for the previous phase,
    then rotate both nested Jobs while the validation mutex remains held. A Job
    retains its hierarchy relationship, so reusing the old recovery Job with a
    fresh private Job would make the second dual assignment fail.
    #>
    param([ValidateRange(100, 600000)][int]$TimeoutMs = 10000)
    if ($null -eq $script:ValidationGateJob -or
        $null -eq $script:ValidationGateRecoveryJob -or
        [string]::IsNullOrWhiteSpace($script:ValidationGateRecoveryJobName) -or
        [string]::IsNullOrWhiteSpace($script:ValidationGateRecoveryJobSddl)) {
        return $false
    }
    $old = $script:ValidationGateJob
    $oldRecovery = $script:ValidationGateRecoveryJob
    if (-not $old.TerminateAndWait($TimeoutMs) -or
        -not $oldRecovery.TerminateAndWait($TimeoutMs)) {
        return $false
    }
    $old.Dispose()
    $oldRecovery.Dispose()
    $script:ValidationGateJob = $null
    $script:ValidationGateRecoveryJob = $null

    if (-not [NospacekeyKillOnCloseJob]::WaitForNamedJobToDisappear(
            $script:ValidationGateRecoveryJobName, $TimeoutMs)) {
        return $false
    }
    $freshRecovery = [NospacekeyKillOnCloseJob]::new(
        $script:ValidationGateRecoveryJobName,
        $script:ValidationGateRecoveryJobSddl)
    try {
        $fresh = [NospacekeyKillOnCloseJob]::new()
    } catch {
        $freshRecovery.Dispose()
        throw
    }
    $script:ValidationGateRecoveryJob = $freshRecovery
    $script:ValidationGateJob = $fresh
    return $true
}

function Exit-ValidationGateLock {
    if ($null -eq $script:ValidationGateMutex) { return }

    # Do not release the mutex until the job is proven empty. On failure the
    # lock and private job deliberately remain owned until process exit, whose
    # handle teardown triggers KILL_ON_JOB_CLOSE without an externally openable
    # Job object in the lifetime path.
    if ($null -ne $script:ValidationGateJob -and
        -not $script:ValidationGateJob.TerminateAndWait(10000)) {
        throw 'validation job did not drain; mutex remains held fail-closed'
    }
    if ($null -ne $script:ValidationGateJob) {
        $script:ValidationGateJob.Dispose()
        $script:ValidationGateJob = $null
    }
    if ($null -ne $script:ValidationGateRecoveryJob -and
        -not $script:ValidationGateRecoveryJob.TerminateAndWait(10000)) {
        throw 'validation recovery job did not drain; mutex remains held fail-closed'
    }
    if ($null -ne $script:ValidationGateRecoveryJob) {
        $script:ValidationGateRecoveryJob.Dispose()
        $script:ValidationGateRecoveryJob = $null
        $script:ValidationGateRecoveryJobName = $null
        $script:ValidationGateRecoveryJobSddl = $null
    }
    if ($null -ne $script:ValidationGateRecoveryLease) {
        $script:ValidationGateRecoveryLease.Dispose()
        $script:ValidationGateRecoveryLease = $null
    }
    $script:ValidationGateMutex.ReleaseMutex()
    $script:ValidationGateMutex.Dispose()
    $script:ValidationGateMutex = $null
    $script:ValidationGateLeaseAccepted = $false
}

function Test-IsAdmin {
    return ([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()
        ).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Invoke-AdminRelaunch {
    <#
    管理者 PowerShell から、指定スクリプトを同一権限の supervisor 配下へ
    開き直す。自動 UAC は行わない。ユーザー書込可能な作業ツリーを RunAs の
    昇格境界へ渡さず、管理者が明示的にこの作業ツリーを信頼した場合だけ実行する。

    supervisor が mutex を所有し、対象を実行開始前に KILL_ON_JOB_CLOSE Job へ
    割り当てる。対象・元の起動シェルの異常終了に lock/子孫回収を依存させない。

    使い方（スクリプト冒頭）:
      Invoke-AdminRelaunch -ScriptPath $PSCommandPath -BoundParameters $PSBoundParameters
      # supervisor 内の対象として戻った後は管理者前提で続行してよい。
    #>
    param(
        [Parameter(Mandatory)][string]$ScriptPath,
        [System.Collections.IDictionary]$BoundParameters,
        [ValidateRange(1, 86400)][int]$TimeoutSec = 3600
    )
    $allowedAdminTargets = @(
        'verify-manual.ps1', 'verify-harness.ps1', 'verify-sp6b.ps1' |
            ForEach-Object { [IO.Path]::GetFullPath((Join-Path $PSScriptRoot $_)) })
    $requestedTarget = [IO.Path]::GetFullPath($ScriptPath)
    if (-not @($allowedAdminTargets | Where-Object {
        $_.Equals($requestedTarget, [StringComparison]::OrdinalIgnoreCase)
    })) {
        throw "refusing an unrecognized elevated validation target: $requestedTarget"
    }
    Invoke-SupervisedValidationRelaunch -ScriptPath $requestedTarget `
        -BoundParameters $BoundParameters -TimeoutSec $TimeoutSec -RequireAdmin
}

function Test-ValidationGateLeaseClaim {
    return -not [string]::IsNullOrWhiteSpace(
        [string]$env:NOSPACEKEY_VALIDATION_GATE_LEASE_ID) -or
        -not [string]::IsNullOrWhiteSpace(
            [string]$env:NOSPACEKEY_VALIDATION_GATE_SUPERVISOR_PID) -or
        -not [string]::IsNullOrWhiteSpace(
            [string]$env:NOSPACEKEY_VALIDATION_GATE_SCOPE)
}

function ConvertTo-ValidationInvocationBase64 {
    param(
        [Parameter(Mandatory)][string]$ScriptPath,
        [System.Collections.IDictionary]$BoundParameters
    )
    $records = @()
    if ($BoundParameters) {
        foreach ($kv in $BoundParameters.GetEnumerator()) {
            $name = [string]$kv.Key
            if ($name -notmatch '^[A-Za-z][A-Za-z0-9]*$') {
                throw "invalid validation parameter name: $name"
            }
            if ($kv.Value -is [System.Management.Automation.SwitchParameter]) {
                $records += [ordered]@{
                    Name = $name
                    Kind = 'Switch'
                    Value = [bool]$kv.Value.IsPresent
                }
            } elseif ($kv.Value -is [Array]) {
                $records += [ordered]@{
                    Name = $name
                    Kind = 'StringArray'
                    Values = @($kv.Value | ForEach-Object { [string]$_ })
                }
            } elseif ($null -eq $kv.Value) {
                $records += [ordered]@{ Name = $name; Kind = 'Null' }
            } else {
                $records += [ordered]@{
                    Name = $name
                    Kind = 'Scalar'
                    Value = [string]$kv.Value
                }
            }
        }
    }
    $payload = [ordered]@{
        Version = 1
        Target = [IO.Path]::GetFullPath($ScriptPath)
        Parameters = $records
    }
    $json = ConvertTo-Json -Depth 6 -Compress -InputObject $payload
    return [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($json))
}

function Invoke-SupervisedValidationRelaunch {
    param(
        [Parameter(Mandatory)][string]$ScriptPath,
        [System.Collections.IDictionary]$BoundParameters,
        [ValidateRange(1, 86400)][int]$TimeoutSec = 3600,
        [switch]$RequireAdmin,
        [switch]$ValidationOnly
    )
    if ($RequireAdmin -and $ValidationOnly) {
        throw 'a validation relaunch cannot be both admin-required and validation-only'
    }

    if (Test-ValidationGateLeaseClaim) {
        if (-not (Enter-ValidationGateLock)) {
            Write-FailMsg 'supervisor lease validation failed; refusing an unsupervised run'
            exit 2
        }
        if ($RequireAdmin) {
            if (-not (Test-IsAdmin)) {
                Write-FailMsg 'the supervised validation target is not elevated'
                exit 2
            }
            Initialize-DirectAdminCleanupContext
        }
        return
    }

    if ($RequireAdmin -and -not (Test-IsAdmin)) {
        Write-FailMsg '管理者権限が必要です。自動 UAC 昇格は安全のため無効です。'
        Write-Host '   「管理者として実行」した PowerShell で同じコマンドを再実行してください。' `
            -ForegroundColor DarkYellow
        exit 2
    }

    $supervisor = Join-Path $PSScriptRoot 'admin-relaunch.ps1'
    if (-not (Test-Path -LiteralPath $supervisor -PathType Leaf)) {
        Write-FailMsg "validation supervisor が見つかりません: $supervisor"
        exit 2
    }
    $encoded = ConvertTo-ValidationInvocationBase64 -ScriptPath $ScriptPath `
        -BoundParameters $BoundParameters

    # 同一権限・同一 pwsh の supervisor を通常 CreateProcess で起動する。
    # UseShellExecute/RunAs を通さないため、作業ツリーは権限境界を越えない。
    $psi = [Diagnostics.ProcessStartInfo]::new()
    $psi.FileName = (Get-Process -Id $PID).Path
    $psi.UseShellExecute = $false
    foreach ($arg in @(
        '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $supervisor,
        '-InvocationBase64', $encoded, '-TimeoutSec', [string]$TimeoutSec)) {
        [void]$psi.ArgumentList.Add($arg)
    }
    if ($ValidationOnly) { [void]$psi.ArgumentList.Add('-ValidationOnly') }

    try { $p = [Diagnostics.Process]::Start($psi) }
    catch {
        Write-FailMsg "validation supervisor を起動できません: $($_.Exception.Message)"
        exit 2
    }
    if ($null -eq $p) {
        Write-FailMsg 'validation supervisor の起動結果が null です'
        exit 2
    }

    # supervisor が対象を TimeoutSec で止め、Job を drain してから終了する。
    # 元のシェルは cleanup 猶予だけ長く待つ。
    if (-not $p.WaitForExit(($TimeoutSec + 45) * 1000)) {
        try {
            if (-not $p.HasExited) { $p.Kill($true); [void]$p.WaitForExit(10000) }
        } catch { }
        Write-FailMsg 'validation supervisor が cleanup 猶予内に終了しませんでした。'
        exit 2
    }
    exit $p.ExitCode
}

function Invoke-ValidationRelaunch {
    <#
    非管理者でも実行できる build/gate を supervisor の Job 配下へ開き直す。
    対象が直接 mutex を所有しないため、対象または元シェルが異常終了しても
    supervisor が全子孫を回収してから mutex を解放する。
    #>
    param(
        [Parameter(Mandatory)][string]$ScriptPath,
        [System.Collections.IDictionary]$BoundParameters,
        [ValidateRange(1, 86400)][int]$TimeoutSec = 3600
    )
    $allowedTargets = @(
        'run-gate.ps1', 'verify-sp6c.ps1', 'verify-sp7.ps1' |
            ForEach-Object { [IO.Path]::GetFullPath((Join-Path $PSScriptRoot $_)) })
    $requestedTarget = [IO.Path]::GetFullPath($ScriptPath)
    if (-not @($allowedTargets | Where-Object {
        $_.Equals($requestedTarget, [StringComparison]::OrdinalIgnoreCase)
    })) {
        throw "refusing an unrecognized validation target: $requestedTarget"
    }
    Invoke-SupervisedValidationRelaunch -ScriptPath $requestedTarget `
        -BoundParameters $BoundParameters -TimeoutSec $TimeoutSec -ValidationOnly
}

# ----------------------------------------------------------------------------
# ツールチェーン環境
# ----------------------------------------------------------------------------

function Import-VcEnv {
    <#
    vcvars64.bat の環境を現在のプロセスへ取り込む（昇格直後シェルは Native Tools
    の env を継いでいないため）。

    vswhere PATH 修正込みの正版: vcvars64.bat は裸の vswhere.exe を呼ぶため、
    VS Installer ディレクトリを PATH 先頭に追加してネスト呼び出しを解決させる。
    （旧 verify-sp6b.ps1 だけが持っていた修正。他の 5 コピーには無く、新規シェル
    での起動に失敗していた。）
    #>
    param([string]$VsWhere = (Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'))
    if ($env:VSCMD_VER) { Write-Ok "MSVC 環境は既に取り込み済み (VS $($env:VSCMD_VER))"; return }
    if (-not (Test-Path $VsWhere)) { throw "vswhere.exe が見つかりません: $VsWhere（VS2022 未導入？）" }
    $vcPath = & $VsWhere -latest -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    if ([string]::IsNullOrWhiteSpace($vcPath)) { throw 'VC x64 ツールが見つかりません（VS Installer で C++ デスクトップ開発を追加）。' }
    $vcvars = Join-Path $vcPath 'VC\Auxiliary\Build\vcvars64.bat'
    if (-not (Test-Path $vcvars)) { throw "vcvars64.bat が見つかりません: $vcvars" }

    $installerDir = Split-Path -Parent $VsWhere
    if ($env:PATH -notlike "*$installerDir*") { $env:PATH = "$installerDir;$env:PATH" }

    cmd /c "call `"$vcvars`" && set" | ForEach-Object {
        if ($_ -match '^([^=]+)=(.*)$') {
            try { Set-Item -Path "Env:\$($matches[1])" -Value $matches[2] -ErrorAction Stop } catch { }
        }
    }
    if (-not $env:VSCMD_VER) { throw 'vcvars64 の取り込みに失敗しました。' }
    Write-Ok "MSVC 環境を取り込みました (VS $($env:VSCMD_VER))"
}

# ----------------------------------------------------------------------------
# エンジン / TIP 登録
# ----------------------------------------------------------------------------

function Get-EngineHostProcs {
    <#現在の対話セッションに属する NospacekeyEngineHost の Process[] を返す
    列挙ヘルパ（Stop-EngineHost の
    初回・最終照合が共通で使う）。

    戻り値: Process[]（対象プロセス無しは空配列 = 正常）。
    例外: 列挙 API の失敗（アクセス拒否等）で throw する。Get-Process -ErrorAction
      SilentlyContinue は「対象無し」と「列挙失敗」を同じ空へ潰してしまうため
      使わない（対象無し = 正常、列挙失敗 = 呼び出し側での fail-closed、を区別）。#>
    $sessionId = [System.Diagnostics.Process]::GetCurrentProcess().SessionId
    return @([System.Diagnostics.Process]::GetProcessesByName(
        'NospacekeyEngineHost') | Where-Object { $_.SessionId -eq $sessionId })
}

function Stop-EngineHost {
    <#現在の対話セッションの NospacekeyEngineHost を停止する（TIP が spawn した子含む）。
    --persist エンジンは自分から終了しない設計（設定アプリCIG対策の常駐）なので
    明示 kill が要る。ログハンドルを握るため、ログ回収前にも必ず呼ぶ。

    戻り値: [bool] を 1 個だけ返す（パイプライン結果を汚さない）。
      $true  = 停止確認済み（最初から不在含む）
      $false = 列挙失敗（初回/最終照合いずれか） / kill 例外 / kill 後 2s 内の
               WaitForExit 失敗 / 再照合での残存
    診断は Write-Host（Write-Warn）のみ。呼び出し側は必ず戻り値を消費して
    fail-closed に扱うこと（teardown=非0 / ビルド・配置前=throw or Fail /
    run-gate=exit 2）。ベアステートメントとしての呼び出しは禁止。#>
    try {
        $procs = @(Get-EngineHostProcs)
    } catch {
        Write-Warn "engine プロセスの列挙に失敗: $($_.Exception.Message)"
        return $false
    }
    if ($procs.Count -eq 0) { return $true }
    $ok = $true
    foreach ($p in $procs) {
        try {
            if (-not $p.HasExited) { $p.Kill() }
            if (-not $p.WaitForExit(2000)) {
                $ok = $false
                Write-Warn "engine (PID $($p.Id)) は kill 後 2s 以内に終了しませんでした"
            }
        } catch {
            $ok = $false
            Write-Warn "engine (PID $($p.Id)) の kill に失敗: $($_.Exception.Message)"
        }
    }
    try {
        $left = @(Get-EngineHostProcs)
    } catch {
        Write-Warn "engine プロセスの再列挙に失敗: $($_.Exception.Message)"
        return $false
    }
    if ($left.Count -gt 0) {
        $ok = $false
        Write-Warn ("engine が残存しています: PID " + (($left | ForEach-Object Id) -join ', '))
    }
    return $ok
}

function ConvertTo-LowerHex([byte[]]$Bytes) {
    return ([BitConverter]::ToString($Bytes) -replace '-', '').ToLowerInvariant()
}

function Get-AdminCleanupMarkerMac {
    param(
        [Parameter(Mandatory)][string]$Token,
        [Parameter(Mandatory)][string]$SourceTipDll,
        [Parameter(Mandatory)][string]$RegisteredTipDll,
        [Parameter(Mandatory)][string]$Sha256,
        [Parameter(Mandatory)][string]$Secret
    )
    $key = [Convert]::FromBase64String($Secret)
    if ($key.Length -ne 32) { throw 'cleanup marker secret must be 256 bits' }
    $payload = "2`n$Token`n$SourceTipDll`n$RegisteredTipDll`n$Sha256"
    $hmac = [Security.Cryptography.HMACSHA256]::new($key)
    try {
        return ConvertTo-LowerHex ($hmac.ComputeHash([Text.Encoding]::UTF8.GetBytes($payload)))
    } finally {
        $hmac.Dispose()
        [Array]::Clear($key, 0, $key.Length)
    }
}

function Test-ValidationCleanupAcl {
    param([Parameter(Mandatory)]$Acl)

    $administrators = 'S-1-5-32-544'
    $system = 'S-1-5-18'
    $readers = @('S-1-5-32-545', 'S-1-15-2-1', 'S-1-15-2-2')
    $expectedRights = @{
        $administrators = [int][Security.AccessControl.FileSystemRights]::FullControl
        $system = [int][Security.AccessControl.FileSystemRights]::FullControl
        'S-1-5-32-545' = 0x1200a9
        'S-1-15-2-1' = 0x1200a9
        'S-1-15-2-2' = 0x1200a9
    }
    try {
        if (-not $Acl.AreAccessRulesProtected -or
            $Acl.GetOwner([Security.Principal.SecurityIdentifier]).Value -ne $administrators) {
            return $false
        }
        $seen = @{}
        foreach ($rule in @($Acl.GetAccessRules(
            $true, $true, [Security.Principal.SecurityIdentifier]))) {
            $sid = $rule.IdentityReference.Value
            if ($rule.IsInherited -or
                $rule.AccessControlType -ne [Security.AccessControl.AccessControlType]::Allow -or
                -not $expectedRights.ContainsKey($sid) -or
                [int]$rule.FileSystemRights -ne [int]$expectedRights[$sid] -or
                $rule.InheritanceFlags -ne (
                    [Security.AccessControl.InheritanceFlags]::ContainerInherit -bor
                    [Security.AccessControl.InheritanceFlags]::ObjectInherit) -or
                $rule.PropagationFlags -ne [Security.AccessControl.PropagationFlags]::None -or
                $seen.ContainsKey($sid)) {
                return $false
            }
            $seen[$sid] = $true
        }
        foreach ($sid in @($administrators, $system) + $readers) {
            if (-not $seen.ContainsKey($sid)) { return $false }
        }
        return $seen.Count -eq $expectedRights.Count
    } catch { return $false }
}

function Test-ValidationCleanupDirectory {
    param(
        [Parameter(Mandatory)][string]$Path,
        [switch]$ForTesting
    )
    try {
        $full = [IO.Path]::GetFullPath($Path).TrimEnd('\')
        $leaf = [IO.Path]::GetFileName($full)
        $expectedRoot = if ($ForTesting) {
            [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\')
        } else {
            [IO.Path]::GetFullPath([Environment]::GetFolderPath(
                [Environment+SpecialFolder]::CommonApplicationData)).TrimEnd('\')
        }
        if ($leaf -notmatch '^nospacekey-admin-cleanup-[0-9a-f]{32}$' -or
            -not [IO.Path]::GetDirectoryName($full).Equals(
                $expectedRoot, [StringComparison]::OrdinalIgnoreCase)) {
            return $false
        }
        $item = Get-Item -LiteralPath $full -Force -ErrorAction Stop
        if (-not $item.PSIsContainer -or
            ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            return $false
        }
        if ($ForTesting) { return $true }

        $acl = Get-Acl -LiteralPath $full -ErrorAction Stop
        return Test-ValidationCleanupAcl -Acl $acl
    } catch { return $false }
}

function New-ValidationCleanupDirectory {
    param(
        [Parameter(Mandatory)][string]$Token,
        [switch]$ForTesting
    )
    if ($Token -notmatch '^[0-9a-f]{32}$') { throw 'cleanup token must be a GUID in N format' }
    $root = if ($ForTesting) {
        [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\')
    } else {
        [IO.Path]::GetFullPath([Environment]::GetFolderPath(
            [Environment+SpecialFolder]::CommonApplicationData)).TrimEnd('\')
    }
    $path = Join-Path $root "nospacekey-admin-cleanup-$Token"
    if (Test-Path -LiteralPath $path) { throw "cleanup directory already exists: $path" }
    if ($ForTesting) {
        [void][IO.Directory]::CreateDirectory($path)
    } else {
        if (-not (Test-IsAdmin)) { throw 'secure cleanup directory requires elevation' }
        Initialize-ValidationGateNative
        [NospacekeyValidationNative]::CreateSecureDirectory(
            $path, $script:ValidationCleanupDirectorySddl)
    }
    if (-not (Test-ValidationCleanupDirectory -Path $path -ForTesting:$ForTesting)) {
        throw "cleanup directory failed ownership/ACL/reparse validation: $path"
    }
    return [IO.Path]::GetFullPath($path)
}

function Remove-ValidationCleanupDirectory {
    param(
        [string]$Path,
        [switch]$ForTesting
    )
    if ([string]::IsNullOrWhiteSpace($Path) -or -not (Test-Path -LiteralPath $Path)) {
        return $true
    }
    try {
        if (-not (Test-ValidationCleanupDirectory -Path $Path -ForTesting:$ForTesting)) {
            throw "refusing unexpected or untrusted cleanup directory: $Path"
        }
        $rootFull = [IO.Path]::GetFullPath($Path)
        $markerFull = [IO.Path]::GetFullPath((Join-Path $rootFull 'marker.json'))
        $markerBytes = $null
        if (Test-Path -LiteralPath $markerFull) {
            $markerItem = Get-Item -LiteralPath $markerFull -Force -ErrorAction Stop
            if ($markerItem.PSIsContainer -or $markerItem.Length -gt 16384 -or
                ($markerItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "cleanup marker is not a bounded regular file: $markerFull"
            }
            # Keep an in-memory recovery copy. If the final root-directory delete
            # fails after marker-last removal, restore the authenticated evidence.
            $markerBytes = [IO.File]::ReadAllBytes($markerFull)
        }
        function Remove-ValidatedCleanupContents(
            [string]$Directory,
            [string]$PreservedMarker
        ) {
            $directoryItem = Get-Item -LiteralPath $Directory -Force -ErrorAction Stop
            if (-not $directoryItem.PSIsContainer -or
                ($directoryItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "cleanup tree contains an unexpected directory: $Directory"
            }
            foreach ($child in @(Get-ChildItem -LiteralPath $Directory -Force -ErrorAction Stop)) {
                if (($child.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                    throw "cleanup directory contains a reparse point: $($child.FullName)"
                }
                if ([IO.Path]::GetFullPath($child.FullName).Equals(
                    $PreservedMarker, [StringComparison]::OrdinalIgnoreCase)) {
                    continue
                }
                if ($child.PSIsContainer) {
                    Remove-ValidatedCleanupContents -Directory $child.FullName `
                        -PreservedMarker $PreservedMarker
                    Remove-Item -LiteralPath $child.FullName -Force -ErrorAction Stop
                } else {
                    Remove-Item -LiteralPath $child.FullName -Force -ErrorAction Stop
                }
            }
        }
        # Any failure while deleting runtime/model contents leaves marker.json in
        # place. It remains the authenticated recovery capability for a later run.
        Remove-ValidatedCleanupContents -Directory $rootFull -PreservedMarker $markerFull
        if (Test-Path -LiteralPath $markerFull) {
            Remove-Item -LiteralPath $markerFull -Force -ErrorAction Stop
        }
        try {
            Remove-Item -LiteralPath $rootFull -Force -ErrorAction Stop
        } catch {
            if ($null -ne $markerBytes -and (Test-Path -LiteralPath $rootFull) -and
                -not (Test-Path -LiteralPath $markerFull)) {
                try { [IO.File]::WriteAllBytes($markerFull, $markerBytes) } catch {
                    Write-Warn "cleanup marker restoration also failed: $($_.Exception.Message)"
                }
            }
            throw
        }
        return $true
    } catch {
        Write-Warn "cleanup directory removal failed: $($_.Exception.Message)"
        return $false
    }
}

function Copy-PinnedValidationFile {
    param(
        [Parameter(Mandatory)]$PinnedSource,
        [Parameter(Mandatory)][string]$Destination,
        [switch]$KeepOpen
    )
    $destinationFull = [IO.Path]::GetFullPath($Destination)
    $stream = [IO.File]::Open($destinationFull, [IO.FileMode]::CreateNew,
        [IO.FileAccess]::ReadWrite, [IO.FileShare]::Read)
    try {
        $PinnedSource.Stream.Position = 0
        $PinnedSource.Stream.CopyTo($stream)
        $stream.Flush($true)
        $stream.Position = 0
        $sha = [Security.Cryptography.SHA256]::Create()
        try { $destinationHash = ConvertTo-LowerHex ($sha.ComputeHash($stream)) }
        finally { $sha.Dispose() }
        if ($destinationHash -cne $PinnedSource.Sha256) {
            throw "protected snapshot hash mismatch: $destinationFull"
        }
        $stream.Position = 0
        $result = [pscustomobject]@{
            Path = $destinationFull
            Sha256 = $destinationHash
            Stream = if ($KeepOpen) { $stream } else { $null }
        }
        if ($KeepOpen) { $stream = $null }
        return $result
    } catch {
        Remove-Item -LiteralPath $destinationFull -Force -ErrorAction SilentlyContinue
        throw
    } finally {
        if ($null -ne $stream) { $stream.Dispose() }
    }
}

function Copy-ValidationRuntimeFile {
    param(
        [Parameter(Mandatory)][string]$Source,
        [Parameter(Mandatory)][string]$Destination,
        [Parameter(Mandatory)][string]$TrustedRoot
    )
    $destinationFull = [IO.Path]::GetFullPath($Destination)
    $destinationDirectory = [IO.Path]::GetDirectoryName($destinationFull)
    $destinationDirectoryItem = Get-Item -LiteralPath $destinationDirectory `
        -Force -ErrorAction Stop
    if (-not $destinationDirectoryItem.PSIsContainer -or
        ($destinationDirectoryItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "runtime destination parent is not a regular directory: $destinationDirectory"
    }
    if (Test-Path -LiteralPath $destinationFull) {
        $existing = Get-Item -LiteralPath $destinationFull -Force -ErrorAction Stop
        if ($existing.PSIsContainer -or
            ($existing.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "runtime destination is not a regular file: $destinationFull"
        }
    }

    $pinned = Open-PinnedValidationTipDll -TipDll $Source `
        -ExpectedPath $Source -TrustedRoot $TrustedRoot
    $temporary = Join-Path $destinationDirectory (
        '.{0}.{1}.tmp' -f ([IO.Path]::GetFileName($destinationFull)),
        [Guid]::NewGuid().ToString('N'))
    try {
        [void](Copy-PinnedValidationFile -PinnedSource $pinned -Destination $temporary)
        [IO.File]::Move($temporary, $destinationFull, $true)
        $destinationHash = (Get-FileHash -LiteralPath $destinationFull `
            -Algorithm SHA256 -ErrorAction Stop).Hash.ToLowerInvariant()
        if ($destinationHash -cne $pinned.Sha256) {
            throw "runtime staging hash mismatch: $destinationFull"
        }
    } finally {
        $pinned.Stream.Dispose()
        if (Test-Path -LiteralPath $temporary) {
            Remove-Item -LiteralPath $temporary -Force -ErrorAction SilentlyContinue
        }
    }
}

function Copy-ValidationRuntimeTree {
    param(
        [Parameter(Mandatory)][string]$SourceDirectory,
        [Parameter(Mandatory)][string]$DestinationDirectory,
        [Parameter(Mandatory)][string]$TrustedRoot
    )
    $sourceFull = [IO.Path]::GetFullPath($SourceDirectory)
    $destinationFull = [IO.Path]::GetFullPath($DestinationDirectory)
    $sourceItem = Get-Item -LiteralPath $sourceFull -Force -ErrorAction Stop
    if (-not $sourceItem.PSIsContainer -or
        ($sourceItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "runtime source is not a regular directory: $sourceFull"
    }
    if (Test-Path -LiteralPath $destinationFull) {
        $destinationItem = Get-Item -LiteralPath $destinationFull -Force -ErrorAction Stop
        if (-not $destinationItem.PSIsContainer -or
            ($destinationItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "runtime destination is not a regular directory: $destinationFull"
        }
    } else {
        [void][IO.Directory]::CreateDirectory($destinationFull)
    }
    foreach ($child in @(Get-ChildItem -LiteralPath $sourceFull -Force -ErrorAction Stop)) {
        if (($child.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "reparse point in runtime source tree: $($child.FullName)"
        }
        $destinationChild = Join-Path $destinationFull $child.Name
        if ($child.PSIsContainer) {
            Copy-ValidationRuntimeTree -SourceDirectory $child.FullName `
                -DestinationDirectory $destinationChild -TrustedRoot $TrustedRoot
        } else {
            Copy-ValidationRuntimeFile -Source $child.FullName `
                -Destination $destinationChild -TrustedRoot $TrustedRoot
        }
    }
}

function Stage-ValidationRuntimeClosure {
    <#
    Materialize the runtime layout used by a real TIP host before protected
    registration. find-swift.ps1 only changes the validation process PATH; a
    medium/AppContainer host that later spawns the sibling engine does not
    inherit it, so all load-time DLLs and SwiftPM resources must be app-local.
    #>
    param(
        [Parameter(Mandatory)][string]$EngineBuildDirectory,
        [Parameter(Mandatory)][string]$DeployDirectory,
        [string]$SwiftRuntimeDirectory,
        [string]$VendorRuntimeDirectory = (Join-Path $script:ValidationRepoRoot `
            'engine-host\vendor\llama\vulkan'),
        [string]$VcRuntimeDirectory,
        [switch]$ForTesting
    )
    $deployFull = [IO.Path]::GetFullPath($DeployDirectory)
    $engineBuildFull = [IO.Path]::GetFullPath($EngineBuildDirectory)
    $vendorFull = [IO.Path]::GetFullPath($VendorRuntimeDirectory)
    if (-not $ForTesting) {
        $expectedDeploy = [IO.Path]::GetFullPath((Join-Path $script:ValidationRepoRoot `
            'target\release'))
        $allowedEngineBuilds = @('debug', 'release') | ForEach-Object {
            [IO.Path]::GetFullPath((Join-Path $script:ValidationRepoRoot `
                "engine-host\.build\x86_64-unknown-windows-msvc\$_"))
        }
        $expectedVendor = [IO.Path]::GetFullPath((Join-Path $script:ValidationRepoRoot `
            'engine-host\vendor\llama\vulkan'))
        if (-not $deployFull.Equals($expectedDeploy, [StringComparison]::OrdinalIgnoreCase) -or
            -not @($allowedEngineBuilds | Where-Object {
                $_.Equals($engineBuildFull, [StringComparison]::OrdinalIgnoreCase)
            }) -or
            -not $vendorFull.Equals($expectedVendor, [StringComparison]::OrdinalIgnoreCase)) {
            throw 'validation runtime sources/layout are outside the fixed repository paths'
        }
    }
    $deployItem = Get-Item -LiteralPath $deployFull -Force -ErrorAction Stop
    if (-not $deployItem.PSIsContainer -or
        ($deployItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "validation runtime destination is not a regular directory: $deployFull"
    }

    if ([string]::IsNullOrWhiteSpace($SwiftRuntimeDirectory)) {
        $swiftRuntimeRoot = Join-Path $env:LOCALAPPDATA 'Programs\Swift\Runtimes'
        $runtimeVersion = Get-ChildItem -LiteralPath $swiftRuntimeRoot -Directory `
            -ErrorAction SilentlyContinue | Sort-Object Name -Descending | Select-Object -First 1
        if ($null -eq $runtimeVersion) {
            throw "Swift runtime not found below: $swiftRuntimeRoot"
        }
        $SwiftRuntimeDirectory = Join-Path $runtimeVersion.FullName 'usr\bin'
    }
    $swiftRuntimeFull = [IO.Path]::GetFullPath($SwiftRuntimeDirectory)
    $swiftDlls = @(Get-ChildItem -LiteralPath $swiftRuntimeFull -Filter '*.dll' `
        -File -Force -ErrorAction Stop)
    if ($swiftDlls.Count -eq 0) { throw "no Swift runtime DLLs found: $swiftRuntimeFull" }
    # Keep this list aligned with the release EngineHost PE's direct imports.
    # Copying every runtime DLL supplies transitive dependencies; these direct
    # names are mandatory so a partial runtime cannot be masked by stale deploy
    # files from an earlier validation.
    foreach ($requiredSwift in $script:ValidationRequiredSwiftDllNames) {
        if ($swiftDlls.Name -notcontains $requiredSwift) {
            throw "required Swift runtime source DLL is missing: $requiredSwift"
        }
    }
    foreach ($dll in $swiftDlls) {
        Copy-ValidationRuntimeFile -Source $dll.FullName `
            -Destination (Join-Path $deployFull $dll.Name) -TrustedRoot $swiftRuntimeFull
    }
    foreach ($requiredSwift in @(
        'swiftCore.dll', 'swift_Concurrency.dll', 'Foundation.dll',
        'swiftWinSDK.dll', 'swiftCRT.dll', 'BlocksRuntime.dll')) {
        if (-not (Test-Path -LiteralPath (Join-Path $deployFull $requiredSwift) -PathType Leaf)) {
            throw "required Swift runtime DLL was not staged: $requiredSwift"
        }
    }

    foreach ($resourceName in $script:ValidationSwiftResourceNames) {
        $resourceSource = Join-Path $engineBuildFull $resourceName
        if (-not (Test-Path -LiteralPath $resourceSource -PathType Container)) {
            throw "required SwiftPM resource bundle is missing: $resourceSource"
        }
        Copy-ValidationRuntimeTree -SourceDirectory $resourceSource `
            -DestinationDirectory (Join-Path $deployFull $resourceName) `
            -TrustedRoot $engineBuildFull
    }

    $zenzaiRuntime = Assert-ZenzaiVulkanRuntimeBundle -RuntimeDirectory $vendorFull `
        -RequireExactDllSet -ForTesting:$ForTesting
    foreach ($dll in $zenzaiRuntime.Dlls) {
        Copy-ValidationRuntimeFile -Source $dll.Path `
            -Destination (Join-Path $deployFull $dll.Name) -TrustedRoot $vendorFull
    }
    Copy-ValidationRuntimeFile -Source $zenzaiRuntime.ManifestPath `
        -Destination (Join-Path $deployFull (Get-ZenzaiVulkanRuntimeContract).ManifestName) `
        -TrustedRoot $vendorFull
    Copy-ValidationRuntimeFile -Source $zenzaiRuntime.ReceiptPath `
        -Destination (Join-Path $deployFull (Get-ZenzaiVulkanRuntimeContract).ReceiptName) `
        -TrustedRoot $vendorFull
    Assert-ZenzaiVulkanRuntimeBundle -RuntimeDirectory $deployFull `
        -ForTesting:$ForTesting | Out-Null

    if (-not $ForTesting -and -not [string]::IsNullOrWhiteSpace($VcRuntimeDirectory)) {
        throw 'VcRuntimeDirectory is accepted only by the focused test harness'
    }
    $vcTrustedRoot = if ($ForTesting) {
        if ([string]::IsNullOrWhiteSpace($VcRuntimeDirectory)) {
            throw 'VcRuntimeDirectory is required for focused runtime-staging tests'
        }
        [IO.Path]::GetFullPath($VcRuntimeDirectory)
    } else {
        # Environment variables are inherited across elevation and are therefore
        # not trust anchors. Environment.SystemDirectory calls GetSystemDirectory
        # and supplies the canonical OS-owned runtime source.
        [IO.Path]::GetFullPath([Environment]::SystemDirectory)
    }

    foreach ($vcName in $script:ValidationCanonicalVcDllNames) {
        $vcDestination = Join-Path $deployFull $vcName
        $vcSource = Join-Path $vcTrustedRoot $vcName
        if (-not (Test-Path -LiteralPath $vcSource -PathType Leaf)) {
            throw "required VC runtime DLL was not found: $vcName"
        }
        Copy-ValidationRuntimeFile -Source $vcSource -Destination $vcDestination `
            -TrustedRoot $vcTrustedRoot
    }
    return [pscustomobject]@{
        SwiftDllCount = $swiftDlls.Count
        VendorDllCount = $zenzaiRuntime.Dlls.Count
        ResourceCount = $script:ValidationSwiftResourceNames.Count
    }
}

function Copy-ProtectedRuntimeTree {
    param(
        [Parameter(Mandatory)][string]$SourceDirectory,
        [Parameter(Mandatory)][string]$DestinationDirectory,
        [Parameter(Mandatory)][string]$TrustedRoot
    )
    $sourceFull = [IO.Path]::GetFullPath($SourceDirectory)
    $destinationFull = [IO.Path]::GetFullPath($DestinationDirectory)
    $sourceItem = Get-Item -LiteralPath $sourceFull -Force -ErrorAction Stop
    if (-not $sourceItem.PSIsContainer -or
        ($sourceItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "runtime source is not a regular directory: $sourceFull"
    }
    if (Test-Path -LiteralPath $destinationFull) {
        throw "protected runtime destination already exists: $destinationFull"
    }
    [void][IO.Directory]::CreateDirectory($destinationFull)
    foreach ($child in @(Get-ChildItem -LiteralPath $sourceFull -Force -ErrorAction Stop)) {
        if (($child.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "reparse point in runtime source tree: $($child.FullName)"
        }
        $destinationChild = Join-Path $destinationFull $child.Name
        if ($child.PSIsContainer) {
            Copy-ProtectedRuntimeTree -SourceDirectory $child.FullName `
                -DestinationDirectory $destinationChild -TrustedRoot $TrustedRoot
            continue
        }
        $pinned = Open-PinnedValidationTipDll -TipDll $child.FullName `
            -ExpectedPath $child.FullName -TrustedRoot $TrustedRoot
        try {
            [void](Copy-PinnedValidationFile -PinnedSource $pinned `
                -Destination $destinationChild)
        } finally {
            $pinned.Stream.Dispose()
        }
    }
}

function New-ProtectedTipSnapshot {
    param(
        [Parameter(Mandatory)]$PinnedTip,
        [Parameter(Mandatory)][string]$CleanupDirectory
    )
    if (-not (Test-ValidationCleanupDirectory -Path $CleanupDirectory `
        -ForTesting:$script:AllowInsecureCleanupDirectoryForTesting)) {
        throw "untrusted protected staging directory: $CleanupDirectory"
    }
    $snapshot = $null
    try {
        $snapshot = Copy-PinnedValidationFile -PinnedSource $PinnedTip `
            -Destination (Join-Path $CleanupDirectory 'nospacekey_tip.dll') -KeepOpen
        Install-VersionLifetimeSentinel -Directory $CleanupDirectory | Out-Null

        # The TIP resolves both executables from its own module directory. Keep
        # the protected registration self-consistent with the runtime layout the
        # validation scripts prepared beside the workspace DLL.
        $sourceDirectory = [IO.Path]::GetDirectoryName($PinnedTip.Path)
        $engineSource = Join-Path $sourceDirectory 'NospacekeyEngineHost.exe'
        if (-not (Test-Path -LiteralPath $engineSource -PathType Leaf)) {
            throw "co-located engine is required before TIP registration: $engineSource"
        }
        $runtimeFiles = @($engineSource)
        $configSource = Join-Path $sourceDirectory 'NospacekeyConfig.exe'
        if (-not (Test-Path -LiteralPath $configSource -PathType Leaf)) {
            throw "co-located config app is required before TIP registration: $configSource"
        }
        $runtimeFiles += $configSource
        $runtimeFiles += @(Get-ChildItem -LiteralPath $sourceDirectory -Filter '*.dll' `
            -File -Force -ErrorAction Stop | Where-Object {
                -not $_.FullName.Equals($PinnedTip.Path, [StringComparison]::OrdinalIgnoreCase) -and
                $script:ValidationCanonicalVcDllNames -notcontains $_.Name
            } | ForEach-Object FullName)
        foreach ($sourcePath in $runtimeFiles) {
            $pinned = Open-PinnedValidationTipDll -TipDll $sourcePath `
                -ExpectedPath $sourcePath -TrustedRoot $script:ValidationRepoRoot
            try {
                $destinationPath = Join-Path $CleanupDirectory `
                    ([IO.Path]::GetFileName($sourcePath))
                [void](Copy-PinnedValidationFile -PinnedSource $pinned `
                    -Destination $destinationPath)
            } finally {
                $pinned.Stream.Dispose()
            }
        }

        # Never re-read canonical VC dependencies from the user-writable deploy
        # directory. Pin and copy them straight from the OS directory at the
        # registration boundary so a post-build replacement cannot be elevated
        # through regsvr32's app-local DLL search.
        $canonicalVcRoot = [IO.Path]::GetFullPath([Environment]::SystemDirectory)
        foreach ($vcName in $script:ValidationCanonicalVcDllNames) {
            $vcSource = Join-Path $canonicalVcRoot $vcName
            $pinned = Open-PinnedValidationTipDll -TipDll $vcSource `
                -ExpectedPath $vcSource -TrustedRoot $canonicalVcRoot
            try {
                [void](Copy-PinnedValidationFile -PinnedSource $pinned `
                    -Destination (Join-Path $CleanupDirectory $vcName))
            } finally {
                $pinned.Stream.Dispose()
            }
        }

        # Preserve relative runtime data when the validation layout contains it.
        # SwiftPM debug/release builds can use their compiled absolute fallback;
        # packaged resource trees and the optional sibling model keep their
        # existing relative shape here.
        $runtimeDirectories = @(Get-ChildItem -LiteralPath $sourceDirectory -Directory `
            -Force -ErrorAction Stop | Where-Object {
                $_.Name -eq 'models' -or $_.Name -like '*.resources'
            })
        foreach ($directory in $runtimeDirectories) {
            Copy-ProtectedRuntimeTree -SourceDirectory $directory.FullName `
                -DestinationDirectory (Join-Path $CleanupDirectory $directory.Name) `
                -TrustedRoot $script:ValidationRepoRoot
        }
        return $snapshot
    } catch {
        if ($null -ne $snapshot -and $null -ne $snapshot.Stream) {
            $snapshot.Stream.Dispose()
        }
        throw
    }
}

function Test-AdminCleanupEnvironment {
    $directory = [string]$env:NOSPACEKEY_ADMIN_CLEANUP_DIRECTORY
    $marker = [string]$env:NOSPACEKEY_ADMIN_CLEANUP_MARKER
    $token = [string]$env:NOSPACEKEY_ADMIN_CLEANUP_TOKEN
    $secret = [string]$env:NOSPACEKEY_ADMIN_CLEANUP_SECRET
    if ($token -notmatch '^[0-9a-f]{32}$' -or
        -not (Test-ValidationCleanupDirectory -Path $directory `
            -ForTesting:$script:AllowInsecureCleanupDirectoryForTesting)) {
        return $false
    }
    try { $secretBytes = [Convert]::FromBase64String($secret) } catch { return $false }
    try {
        if ($secretBytes.Length -ne 32) { return $false }
    } finally { [Array]::Clear($secretBytes, 0, $secretBytes.Length) }
    $expectedMarker = [IO.Path]::GetFullPath((Join-Path $directory 'marker.json'))
    try {
        return [IO.Path]::GetFullPath($marker).Equals(
            $expectedMarker, [StringComparison]::OrdinalIgnoreCase)
    } catch { return $false }
}

function Initialize-DirectAdminCleanupContext {
    if ($script:ValidationGateLeaseAccepted) {
        if (-not (Test-AdminCleanupEnvironment)) {
            throw 'supervisor cleanup environment failed validation'
        }
        return
    }
    $token = [Guid]::NewGuid().ToString('N')
    $secretBytes = [Security.Cryptography.RandomNumberGenerator]::GetBytes(32)
    try { $secret = [Convert]::ToBase64String($secretBytes) }
    finally { [Array]::Clear($secretBytes, 0, $secretBytes.Length) }
    $directory = New-ValidationCleanupDirectory -Token $token
    $script:ValidationCleanupDirectory = $directory
    $env:NOSPACEKEY_ADMIN_CLEANUP_DIRECTORY = $directory
    $env:NOSPACEKEY_ADMIN_CLEANUP_MARKER = Join-Path $directory 'marker.json'
    $env:NOSPACEKEY_ADMIN_CLEANUP_TOKEN = $token
    $env:NOSPACEKEY_ADMIN_CLEANUP_SECRET = $secret
}

function Open-PinnedValidationTipDll {
    param(
        [Parameter(Mandatory)][string]$TipDll,
        [string]$ExpectedPath = $script:ValidationTipDll,
        [string]$TrustedRoot = $script:ValidationRepoRoot
    )
    $full = [IO.Path]::GetFullPath($TipDll)
    $expected = [IO.Path]::GetFullPath($ExpectedPath)
    $root = [IO.Path]::GetFullPath($TrustedRoot).TrimEnd('\')
    if (-not [IO.Path]::IsPathRooted($TipDll) -or $full.StartsWith('\\')) {
        throw "TIP DLL must be a rooted local path: $TipDll"
    }
    if (-not $full.Equals($expected, [StringComparison]::OrdinalIgnoreCase)) {
        throw "unexpected TIP DLL path: $full"
    }
    if (-not ($full.Equals($root, [StringComparison]::OrdinalIgnoreCase) -or
        $full.StartsWith(($root + '\'), [StringComparison]::OrdinalIgnoreCase))) {
        throw "TIP DLL is outside the trusted root: $full"
    }

    $cursor = $full
    while ($true) {
        $item = Get-Item -LiteralPath $cursor -Force -ErrorAction Stop
        if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "reparse point in TIP DLL path: $cursor"
        }
        if ($cursor.Equals($root, [StringComparison]::OrdinalIgnoreCase)) { break }
        $parent = [IO.Path]::GetDirectoryName($cursor.TrimEnd('\'))
        if ([string]::IsNullOrWhiteSpace($parent) -or $parent -eq $cursor) {
            throw "trusted root is not an ancestor of TIP DLL: $root"
        }
        $cursor = $parent
    }

    # Open first with FILE_FLAG_OPEN_REPARSE_POINT, then validate the opened
    # handle's attributes and normalized final path. The earlier path walk gives
    # useful diagnostics; this handle check closes its check-to-open race.
    Initialize-ValidationGateNative
    $stream = [NospacekeyValidationNative]::OpenPinnedReadFile($full, $root)
    try {
        $sha = [Security.Cryptography.SHA256]::Create()
        try { $hash = ConvertTo-LowerHex ($sha.ComputeHash($stream)) }
        finally { $sha.Dispose() }
        $stream.Position = 0
        return [pscustomobject]@{ Path = $full; Sha256 = $hash; Stream = $stream }
    } catch {
        $stream.Dispose()
        throw
    }
}

function Invoke-BoundedRegsvr32 {
    param(
        [Parameter(Mandatory)][string]$TipDll,
        [switch]$Unregister,
        [ValidateRange(100, 600000)][int]$TimeoutMs = 10000
    )
    $regsvr = Join-Path ([Environment]::SystemDirectory) 'regsvr32.exe'
    $process = $null
    $launched = $false
    $timedOut = $false
    $killConfirmed = $true
    $exitCode = 2
    $failure = ''
    try {
        $psi = [Diagnostics.ProcessStartInfo]::new()
        $psi.FileName = $regsvr
        $psi.UseShellExecute = $false
        $psi.CreateNoWindow = $true
        if ($Unregister) { [void]$psi.ArgumentList.Add('/u') }
        [void]$psi.ArgumentList.Add('/s')
        [void]$psi.ArgumentList.Add($TipDll)
        if ($null -ne $script:ValidationGateJob) {
            $process = $script:ValidationGateJob.StartProcess(
                $regsvr, [string[]]@($psi.ArgumentList),
                $script:ValidationGateRecoveryJob)
        } else {
            $process = [Diagnostics.Process]::Start($psi)
        }
        if ($null -eq $process) { throw 'process start returned null' }
        $launched = $true
        if ($process.WaitForExit($TimeoutMs)) {
            $exitCode = $process.ExitCode
        } else {
            $timedOut = $true
            $killConfirmed = $false
            try {
                $process.Kill($true)
                $killConfirmed = $process.WaitForExit(5000) -and $process.HasExited
            } catch { $failure = $_.Exception.Message }
            if (-not $failure) { $failure = "timeout after ${TimeoutMs}ms" }
        }
    } catch {
        $failure = $_.Exception.Message
        if ($null -ne $process -and -not $process.HasExited) {
            try {
                $process.Kill($true)
                $killConfirmed = $process.WaitForExit(5000) -and $process.HasExited
            } catch { $killConfirmed = $false }
        }
    } finally {
        if ($null -ne $process) { $process.Dispose() }
    }
    return [pscustomobject]@{
        ExitCode = $exitCode
        Launched = $launched
        TimedOut = $timedOut
        KillConfirmed = $killConfirmed
        Failure = $failure
    }
}

function Get-RegisteredTipDllPath {
    try {
        $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey(
            [Microsoft.Win32.RegistryHive]::LocalMachine,
            [Microsoft.Win32.RegistryView]::Registry64)
        try {
            # HKEY_CLASSES_ROOT is a merged HKLM/HKCU view. Never let a
            # per-user Classes overlay choose a DLL that elevated cleanup will
            # load; validation registration is machine-wide.
            $key = $base.OpenSubKey(
                "SOFTWARE\Classes\CLSID\$($script:NospacekeyClsid)\InprocServer32",
                $false)
            if ($null -eq $key) { return $null }
            try { return [string]$key.GetValue('', $null,
                [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames) }
            finally { $key.Dispose() }
        } finally { $base.Dispose() }
    } catch { throw "registered TIP path query failed: $($_.Exception.Message)" }
}

function Test-TipRegistrationAbsent {
    $checks = @(
        @([Microsoft.Win32.RegistryHive]::LocalMachine,
            "SOFTWARE\Classes\CLSID\$($script:NospacekeyClsid)"),
        # Older builds wrote through merged HKCR. A pre-existing per-user key
        # could therefore receive the value and would override the machine COM
        # mapping even after HKLM cleanup.
        @([Microsoft.Win32.RegistryHive]::CurrentUser,
            "Software\Classes\CLSID\$($script:NospacekeyClsid)"),
        @([Microsoft.Win32.RegistryHive]::LocalMachine,
            "SOFTWARE\Microsoft\CTF\TIP\$($script:NospacekeyClsid)"),
        @([Microsoft.Win32.RegistryHive]::LocalMachine,
            "SOFTWARE\WOW6432Node\Microsoft\CTF\TIP\$($script:NospacekeyClsid)"))
    try {
        foreach ($check in $checks) {
            $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey(
                $check[0], [Microsoft.Win32.RegistryView]::Registry64)
            try {
                $key = $base.OpenSubKey([string]$check[1], $false)
                if ($null -ne $key) { $key.Dispose(); return $false }
            } finally { $base.Dispose() }
        }
        return $true
    } catch {
        Write-Warn "TIP registry absence could not be verified: $($_.Exception.Message)"
        return $false
    }
}

function Remove-NospacekeyMachineRegistration {
    <#
    Remove the fixed machine-owned registry trees without loading a DLL, plus
    the exact current-user CLSID tree that legacy HKCR-based builds could have
    created. This is the safe recovery path for legacy workspace registrations
    and partial registrations whose InprocServer32 is missing or untrusted.
    #>
    if (-not (Test-IsAdmin)) {
        Write-Warn 'machine TIP registry cleanup requires elevation'
        return $false
    }
    $paths = @(
        @([Microsoft.Win32.RegistryHive]::LocalMachine,
            "SOFTWARE\Classes\CLSID\$($script:NospacekeyClsid)"),
        @([Microsoft.Win32.RegistryHive]::CurrentUser,
            "Software\Classes\CLSID\$($script:NospacekeyClsid)"),
        @([Microsoft.Win32.RegistryHive]::LocalMachine,
            "SOFTWARE\Microsoft\CTF\TIP\$($script:NospacekeyClsid)"),
        @([Microsoft.Win32.RegistryHive]::LocalMachine,
            "SOFTWARE\WOW6432Node\Microsoft\CTF\TIP\$($script:NospacekeyClsid)"))
    $ok = $true
    foreach ($entry in $paths) {
        $hive = $entry[0]
        $path = [string]$entry[1]
        $base = $null
        try {
            $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey(
                $hive,
                [Microsoft.Win32.RegistryView]::Registry64)
            $base.DeleteSubKeyTree($path, $false)
        } catch {
            Write-Warn "TIP registry cleanup failed ($hive\$path): $($_.Exception.Message)"
            $ok = $false
        } finally {
            if ($null -ne $base) { $base.Dispose() }
        }
    }
    return $ok -and (Test-TipRegistrationAbsent)
}

function Clear-AdminCleanupEnvironment {
    foreach ($name in @(
        'NOSPACEKEY_ADMIN_CLEANUP_DIRECTORY',
        'NOSPACEKEY_ADMIN_CLEANUP_MARKER',
        'NOSPACEKEY_ADMIN_CLEANUP_TOKEN',
        'NOSPACEKEY_ADMIN_CLEANUP_SECRET')) {
        Remove-Item -LiteralPath "Env:\$name" -ErrorAction SilentlyContinue
    }
}

function Invoke-RegisterTip {
    param([Parameter(Mandatory)][string]$TipDll)
    if (-not (Test-AdminCleanupEnvironment)) {
        throw 'TIP registration requires a validated protected cleanup context'
    }
    if ($null -ne $script:RegisteredTipSnapshot) {
        throw 'a protected TIP snapshot is already active in this validation process'
    }
    if (-not (Test-TipRegistrationAbsent)) {
        Write-Warn 'an existing machine TIP registration must be removed before registration'
        return 2
    }

    Install-VersionLifetimeSentinel `
        -Directory ([IO.Path]::GetDirectoryName([IO.Path]::GetFullPath($TipDll))) | Out-Null

    $source = Open-PinnedValidationTipDll -TipDll $TipDll
    $snapshot = $null
    try {
        # Never let elevated regsvr32 reopen the user-writable workspace path or
        # search its directory for app-local dependency DLLs. It receives only
        # the byte-identical copy in the atomic protected runtime directory.
        # Only Administrators/SYSTEM can write it; TIP host identities receive
        # read/execute so normal and AppContainer activation still works.
        $snapshot = New-ProtectedTipSnapshot -PinnedTip $source `
            -CleanupDirectory $env:NOSPACEKEY_ADMIN_CLEANUP_DIRECTORY
        $script:RegisteredTipSnapshot = $snapshot

        $mac = Get-AdminCleanupMarkerMac -Token $env:NOSPACEKEY_ADMIN_CLEANUP_TOKEN `
            -SourceTipDll $source.Path -RegisteredTipDll $snapshot.Path `
            -Sha256 $snapshot.Sha256 -Secret $env:NOSPACEKEY_ADMIN_CLEANUP_SECRET
        $marker = [pscustomobject]@{
            Version = 2
            Token = $env:NOSPACEKEY_ADMIN_CLEANUP_TOKEN
            SourceTipDll = $source.Path
            RegisteredTipDll = $snapshot.Path
            Sha256 = $snapshot.Sha256
            Mac = $mac
        }
        $markerTemp = "$($env:NOSPACEKEY_ADMIN_CLEANUP_MARKER).$PID.tmp"
        try {
            $marker | ConvertTo-Json -Compress |
                Set-Content -LiteralPath $markerTemp -Encoding utf8 -ErrorAction Stop
            Move-Item -LiteralPath $markerTemp `
                -Destination $env:NOSPACEKEY_ADMIN_CLEANUP_MARKER -ErrorAction Stop
        } finally {
            if (Test-Path -LiteralPath $markerTemp) {
                Remove-Item -LiteralPath $markerTemp -Force -ErrorAction SilentlyContinue
            }
        }

        $register = Invoke-BoundedRegsvr32 -TipDll $snapshot.Path
        if ($register.ExitCode -eq 0) {
            $script:ValidationRegistrationHelperUnconfirmed = $false
            return 0
        }

        $safeToRollback = $register.Launched -and $register.KillConfirmed
        if ($register.Launched -and -not $register.KillConfirmed -and
            $null -ne $script:ValidationGateJob) {
            $script:ValidationRegistrationHelperUnconfirmed = $true
            # Direct-admin ownership: terminate/drain the job containing the
            # first regsvr32, then rotate before starting its inverse.
            $safeToRollback = Reset-ValidationGateJob
            if ($safeToRollback) {
                $script:ValidationRegistrationHelperUnconfirmed = $false
            }
        } elseif ($register.Launched -and -not $register.KillConfirmed) {
            # A supervised target inherits the supervisor's private Job but has
            # no owning handle with which to drain it. Keep the marker and
            # snapshot intact until the supervisor drains the target phase.
            $script:ValidationRegistrationHelperUnconfirmed = $true
        }
        if ($safeToRollback) {
            $rollback = Invoke-BoundedRegsvr32 -TipDll $snapshot.Path -Unregister
            if ($rollback.ExitCode -eq 0 -and (Test-TipRegistrationAbsent)) {
                $snapshotCleanupDirectory = [IO.Path]::GetDirectoryName($snapshot.Path)
                $snapshot.Stream.Dispose()
                $script:RegisteredTipSnapshot = $null
                # Remove-ValidationCleanupDirectory keeps marker.json until every
                # runtime/model entry is gone, then removes it last.
                if (-not (Remove-ValidationCleanupDirectory `
                    -Path $snapshotCleanupDirectory `
                    -ForTesting:$script:AllowInsecureCleanupDirectoryForTesting)) {
                    return 2
                }
                if ($null -ne $script:ValidationCleanupDirectory -and
                    $script:ValidationCleanupDirectory.Equals(
                        $snapshotCleanupDirectory,
                        [StringComparison]::OrdinalIgnoreCase)) {
                    $script:ValidationCleanupDirectory = $null
                    Clear-AdminCleanupEnvironment
                }
                return $register.ExitCode
            }
            Write-Warn "regsvr32 registration failed and checked rollback did not prove a clean registry (register=$($register.ExitCode), unregister=$($rollback.ExitCode))"
        } else {
            Write-Warn 'registration helper termination was not confirmed; protected marker/snapshot retained for supervisor cleanup'
        }
        return 2
    } catch {
        if ($null -ne $snapshot -and
            -not (Test-Path -LiteralPath $env:NOSPACEKEY_ADMIN_CLEANUP_MARKER)) {
            $snapshot.Stream.Dispose()
            $script:RegisteredTipSnapshot = $null
        }
        if (-not (Test-Path -LiteralPath $env:NOSPACEKEY_ADMIN_CLEANUP_MARKER) -and
            -not [string]::IsNullOrWhiteSpace(
                [string]$env:NOSPACEKEY_ADMIN_CLEANUP_DIRECTORY)) {
            [void](Remove-ValidationCleanupDirectory `
                -Path $env:NOSPACEKEY_ADMIN_CLEANUP_DIRECTORY `
                -ForTesting:$script:AllowInsecureCleanupDirectoryForTesting)
            if ($null -ne $script:ValidationCleanupDirectory) {
                $script:ValidationCleanupDirectory = $null
                Clear-AdminCleanupEnvironment
            }
        }
        throw
    } finally {
        $source.Stream.Dispose()
    }
}

function Invoke-UnregisterTip {
    param([Parameter(Mandatory)][string]$TipDll)

    if ($script:ValidationRegistrationHelperUnconfirmed) {
        Write-Warn 'registration helper termination is unconfirmed; deferring cleanup until supervisor job drain'
        return 2
    }

    $registeredPath = Get-RegisteredTipDllPath
    if ([string]::IsNullOrWhiteSpace($registeredPath) -and
        (Test-TipRegistrationAbsent)) {
        if ($null -ne $script:RegisteredTipSnapshot) {
            $completedSnapshotDirectory = [IO.Path]::GetDirectoryName(
                $script:RegisteredTipSnapshot.Path)
            $script:RegisteredTipSnapshot.Stream.Dispose()
            $script:RegisteredTipSnapshot = $null
            if (-not (Remove-ValidationCleanupDirectory -Path $completedSnapshotDirectory `
                -ForTesting:$script:AllowInsecureCleanupDirectoryForTesting)) {
                return 2
            }
            if ($null -ne $script:ValidationCleanupDirectory -and
                $script:ValidationCleanupDirectory.Equals(
                    $completedSnapshotDirectory, [StringComparison]::OrdinalIgnoreCase)) {
                $script:ValidationCleanupDirectory = $null
                Clear-AdminCleanupEnvironment
            }
        }
        # A supervised target's env directory belongs to the supervisor and is
        # intentionally still empty here. Keep it available for a later
        # registration in this same -Rebuild run; the supervisor removes it
        # after the target exits if no marker was ever created.
        return 0
    }

    $pinned = $null
    $ownsPinned = $false
    $externalCleanupDirectory = $null
    $snapshotCleanupDirectory = $null
    $registryOnlyCleanup = $false
    try {
        if ($null -ne $script:RegisteredTipSnapshot) {
            $pinned = $script:RegisteredTipSnapshot
            $snapshotCleanupDirectory = [IO.Path]::GetDirectoryName($pinned.Path)
        } elseif (-not [string]::IsNullOrWhiteSpace($registeredPath)) {
            $registeredFull = [IO.Path]::GetFullPath($registeredPath)
            $sourceFull = [IO.Path]::GetFullPath($TipDll)
            if ($registeredFull.Equals($sourceFull, [StringComparison]::OrdinalIgnoreCase)) {
                # Legacy versions registered the user-writable workspace DLL.
                # Loading it in elevated regsvr32 /u is a confused-deputy path;
                # delete only the fixed machine registry trees instead.
                $registryOnlyCleanup = $true
            } else {
                $registeredDirectory = [IO.Path]::GetDirectoryName($registeredFull)
                if (-not (Test-ValidationCleanupDirectory -Path $registeredDirectory) -or
                    -not $registeredFull.Equals(
                        (Join-Path $registeredDirectory 'nospacekey_tip.dll'),
                        [StringComparison]::OrdinalIgnoreCase)) {
                    Write-Warn "refusing to load an unknown registered TIP path; using registry-only cleanup: $registeredFull"
                    $registryOnlyCleanup = $true
                } elseif (-not (Test-Path -LiteralPath $registeredFull -PathType Leaf)) {
                    Write-Warn "protected registered TIP is missing; using registry-only cleanup: $registeredFull"
                    $externalCleanupDirectory = $registeredDirectory
                    $registryOnlyCleanup = $true
                } else {
                    $pinned = Open-PinnedValidationTipDll -TipDll $registeredFull `
                        -ExpectedPath $registeredFull -TrustedRoot $registeredDirectory
                    $externalCleanupDirectory = $registeredDirectory
                    $ownsPinned = $true
                }
            }
        } else {
            # A partial TSF registration may remain without InprocServer32.
            # Its cleanup must not guess and load a workspace DLL.
            $registryOnlyCleanup = $true
        }

        if ($registryOnlyCleanup) {
            if (-not (Remove-NospacekeyMachineRegistration)) { return 2 }
            $currentContextDirectory = if (Test-AdminCleanupEnvironment) {
                [IO.Path]::GetFullPath(
                    [string]$env:NOSPACEKEY_ADMIN_CLEANUP_DIRECTORY)
            } else { $null }
            if (-not [string]::IsNullOrWhiteSpace($externalCleanupDirectory) -and
                ($null -eq $currentContextDirectory -or
                    -not [IO.Path]::GetFullPath($externalCleanupDirectory).Equals(
                        $currentContextDirectory,
                        [StringComparison]::OrdinalIgnoreCase)) -and
                -not (Remove-ValidationCleanupDirectory -Path $externalCleanupDirectory `
                    -ForTesting:$script:AllowInsecureCleanupDirectoryForTesting)) {
                return 2
            }
            return 0
        }

        $result = Invoke-BoundedRegsvr32 -TipDll $pinned.Path -Unregister
        if ($result.ExitCode -ne 0 -or -not (Test-TipRegistrationAbsent)) {
            return 2
        }

        if ($null -ne $script:RegisteredTipSnapshot) {
            $script:RegisteredTipSnapshot.Stream.Dispose()
            $script:RegisteredTipSnapshot = $null
        }
        if ($ownsPinned -and $null -ne $pinned) {
            $pinned.Stream.Dispose()
            $ownsPinned = $false
        }
        $cleanupDirectories = @($snapshotCleanupDirectory, $externalCleanupDirectory) |
            Where-Object { -not [string]::IsNullOrWhiteSpace([string]$_) } |
            Select-Object -Unique
        foreach ($cleanupDirectory in $cleanupDirectories) {
            if (-not (Remove-ValidationCleanupDirectory -Path $cleanupDirectory `
                -ForTesting:$script:AllowInsecureCleanupDirectoryForTesting)) {
                return 2
            }
        }
        if ($null -ne $script:ValidationCleanupDirectory) {
            if ($cleanupDirectories -notcontains $script:ValidationCleanupDirectory) {
                if (-not (Remove-ValidationCleanupDirectory `
                    -Path $script:ValidationCleanupDirectory)) {
                    return 2
                }
            }
            $script:ValidationCleanupDirectory = $null
            Clear-AdminCleanupEnvironment
        }
        return 0
    } finally {
        if ($ownsPinned -and $null -ne $pinned) { $pinned.Stream.Dispose() }
    }
}

function Show-DllHolders {
    <#指定 DLL をイメージとしてロード中のプロセスを列挙（os error 5 / LNK1104 の
    犯人特定。閉じれば解放される）。保護プロセスは Modules 取得で例外→無視。#>
    param([Parameter(Mandatory)][string]$Path)
    Write-Host '   この DLL をロード中のプロセス（閉じれば解放されます）:' -ForegroundColor DarkYellow
    $leaf = Split-Path $Path -Leaf
    $found = $false
    foreach ($p in Get-Process -ErrorAction SilentlyContinue) {
        try {
            foreach ($m in $p.Modules) {
                if ($m.ModuleName -ieq $leaf) {
                    Write-Host ("     - {0} (PID {1})" -f $p.ProcessName, $p.Id) -ForegroundColor DarkYellow
                    $found = $true; break
                }
            }
        } catch { }
    }
    if (-not $found) { Write-Host '     （特定できませんでした。VM 再起動が確実です）' -ForegroundColor DarkGray }
}

function Invoke-FreeOldTipDll {
    <#
    再ビルド前に旧 nospacekey_tip.dll を解放する。

    登録済み＆どこかのプロセス（explorer / WindowsTerminal / claude 等）にロード中
    だと cargo が DLL を置換できず os error 5 になる。エンジン停止＋登録解除の後、
    削除を試み、ロード中で消せなければ同一ボリューム rename で退避する（Windows は
    ロード中 DLL の rename を許す＝再起動もアプリ終了も不要で同じパスに新 DLL を
    書ける）。削除も退避もできなければ throw（呼び出し側で Fail）。

    ※ハードリンク先 deps\*.dll の退避は free-tip-build-output.ps1 が担う（別呼び出し）。
    #>
    param([Parameter(Mandatory)][string]$TipDll)
    # エンジンを止められないままビルドへ進むと stale engine（旧 DLL をロードした常駐）
    # 相手の検証になるため、stop 失敗は throw で遮断（呼び出し側の catch/Fail へ）。
    if (-not (Stop-EngineHost)) {
        throw 'エンジンを停止できなかった（NospacekeyEngineHost 残存）— 旧 DLL の解放に進めない'
    }
    $unregisterExit = Invoke-UnregisterTip -TipDll $TipDll
    if ($unregisterExit -ne 0) {
        throw "TIP registration cleanup could not be proven (exit $unregisterExit)"
    }
    Start-Sleep -Milliseconds 300
    if (-not (Test-Path $TipDll)) { return }
    try {
        Remove-Item $TipDll -Force -ErrorAction Stop
        Write-Ok '旧 DLL を削除（作り直します）'
        return
    } catch { }
    $stashLeaf = "$(Split-Path $TipDll -Leaf).$(Get-Date -Format yyyyMMddHHmmss).old"
    try {
        Rename-Item -Path $TipDll -NewName $stashLeaf -ErrorAction Stop
        Write-Ok "旧 DLL はロード中のため退避 → $stashLeaf（再起動不要で作り直します）"
        Show-DllHolders $TipDll
    } catch {
        Show-DllHolders $TipDll
        throw "旧 DLL を解放（削除も退避も）できません: $TipDll （ロード中アプリを閉じるか再起動後に再実行）"
    }
}

# ----------------------------------------------------------------------------
# testbench セーフスポーン（プロジェクト最大の罠の実装版）
# ----------------------------------------------------------------------------

function Stop-TestbenchAndConfirm {
    param(
        [Parameter(Mandatory)][System.Diagnostics.Process]$Process,
        [int]$WaitMs = 2000
    )
    try {
        if ($Process.HasExited) { return $true }
        $Process.Kill()
        if (-not $Process.WaitForExit($WaitMs)) {
            Write-Warn "testbench (PID $($Process.Id)) は kill 後 ${WaitMs}ms 内に終了しませんでした"
            return $false
        }
        return $Process.HasExited
    } catch {
        Write-Warn "testbench (PID $($Process.Id)) の kill/終了確認に失敗: $($_.Exception.Message)"
        return $false
    }
}

function Invoke-Testbench {
    <#
    testbench を「パイプに繋がず・Start-Process -Wait を使わず」実行する正規ランナー。

    何故か: testbench は数秒で完走するが、testbench が spawn した
    `AzooKeyEngineHost.exe ... --persist` は設計上自分から終了しない常駐で、
    testbench の stdout 書き込みハンドルを継承したまま生き続ける。
      - PS パイプ（| Where-Object 等）は EOF（全書き込みハンドル閉鎖）まで読み
        続けるため、--persist エンジンがハンドルを握る限り永久ハング（実測 11 時間）。
      - Start-Process -Wait は子孫プロセスツリー全体を待つため同じくハング。
    正解 = stdout/stderr はファイルへ、待つのは .NET WaitForExit(timeout) で
    testbench プロセス単体のみ（詳細: run-gate.ps1 ヘッダ / CLAUDE.md）。

    戻り値: [pscustomobject]@{ ExitCode = int; TimedOut = bool; KillConfirmed = bool }
    タイムアウト時は testbench を kill して終了を確認し、ExitCode=2 で返す。
    例外: 起動失敗、および WaitForExit / ExitCode 取得の失敗は throw する。
    後者では起動済み testbench 自身が生存していれば best-effort kill してから
    再 throw する（testbench を常駐させない。生存確認・kill の失敗は握り潰し、
    元例外の再 throw を優先）。呼び出し側の catch がエンジン停止を担う。
    #>
    param(
        [Parameter(Mandatory)][string]$Testbench,
        [string[]]$ArgumentList = @(),
        [Parameter(Mandatory)][string]$OutFile,
        [Parameter(Mandatory)][string]$ErrFile,
        [int]$TimeoutSec = 240
    )
    $p = $null
    try {
        # Start-Process -ArgumentList は要素の自動クォートをしない（実測: pwsh 7.6 でも
        # 空白入り要素は相手プロセスで分割される）。空白を含む要素だけクォートして渡す
        # （要素内の " は \" にエスケープ — CommandLineToArgvW 規則）。
        $quoted = @($ArgumentList | ForEach-Object {
            if (($_ -as [string]) -match '\s') { '"{0}"' -f ($_ -replace '"', '\"') } else { $_ }
        })
        $p = Start-Process -FilePath $Testbench -ArgumentList $quoted -PassThru `
            -WindowStyle Hidden -RedirectStandardOutput $OutFile -RedirectStandardError $ErrFile -ErrorAction Stop
    } catch {
        throw "testbench の起動に失敗: $Testbench ($($_.Exception.Message))"
    }
    try {
        if (-not $p.WaitForExit($TimeoutSec * 1000)) {
            $killConfirmed = Stop-TestbenchAndConfirm -Process $p
            return [pscustomobject]@{
                ExitCode = 2; TimedOut = $true; KillConfirmed = $killConfirmed
            }
        }
        return [pscustomobject]@{
            ExitCode = $p.ExitCode; TimedOut = $false; KillConfirmed = $true
        }
    } catch {
        # WaitForExit / ExitCode 取得の例外でも testbench を常駐させない:
        # 生存していれば best-effort kill してから再 throw（kill 失敗は握り潰す）。
        try { if (-not $p.HasExited) { [void](Stop-TestbenchAndConfirm -Process $p) } } catch { }
        throw
    }
}

# ----------------------------------------------------------------------------
# ゲート結果サマリ
# ----------------------------------------------------------------------------

function Get-GateSummary {
    <#
    testbench の stdout 行から結果を集計する。結果行は「ステータス列が PASS / FAIL / ERROR
    のいずれかと厳密一致する」行だけで、それ以外の任意テキスト・未知ステータスは
    成功にも失敗にも数えない（SKIP 等が PASS に化けない）。

    対応する出力形式（正典: crates/testbench/src/report.rs print_table / main.rs 各 mode）:
      scenarios : "  N | ✅ PASS | name — detail"
                  起動失敗行は "  ! | ERROR  | TsfHost 起動失敗: ..."（項番の代わりに "!"）
      smoke 系  : "name : PASS (detail)"  — item8/12/13/... と keymap-smoke のサブ行
                  "item9 PASS" / "CANONICAL PASS" — コロン無し・行全体がトークン+ステータス

    契約:
      - PassCount は PASS 行だけを明示的に数える（items - fails の引き算をしない）。
      - Fails はステータスが FAIL または ERROR と厳密一致した行のみ。
      - ステータスの厳密一致 = scenario 行は前後の "|" で挟まれた列、smoke/bare 行は
        ステータスの直後が空白・"("・行末。"PASSED" 等は不一致として無視される。
      - 解析ゼロ件（ParsedCount=0）は「未知の出力形式＝成功と誤報できない」を意味する。
        呼び出し側は Resolve-GateExit で必ず fail-closed に正規化すること
        （Write-GateSummary は警告表示のみ）。
      - 任意のマーク列（✅ 等）は `[^\s|]+\s+` で status と空白分離して吸収する。
        testbench の stdout は UTF-8 で
        pwsh 7 の Get-Content 既定（utf8NoBOM）なら正しく復号される。
    #>
    param(
        # Mandatory を付けない: 検証は配列の要素レベルにも及び、ゲート出力の先頭の
        # 空行1つでバインドが拒否され run-gate ごとクラッシュしていた（実機実行で
        # 捕獲）。空入力は ParsedCount=0 =「no result lines parsed」警告として扱う。
        [string[]]$Lines = @()
    )
    # 空行・$null 要素を正規化して落とす（どの結果行正規表現にも無関係なため安全）。
    $Lines = @($Lines | Where-Object { $_ })
    # scenario: $matches[1] = item ID / !, $matches[2] = PASS / FAIL / ERROR。
    # optional marker と status の間には空白を必須化し、BOGUSPASS 等へのバックトラックを防ぐ。
    $scenarioRe = '^\s*(\d+|!)\s*\|\s*(?:[^\s|]+\s+)?(PASS|FAIL|ERROR)\s*\|'
    $smokeRe    = '^\S.*\s:\s(PASS|FAIL|ERROR)(?:\s|\(|$)'
    $bareRe     = '^\S+\s(PASS|FAIL|ERROR)\s*$'
    $items = [System.Collections.Generic.List[string]]::new()
    $fails = [System.Collections.Generic.List[string]]::new()
    $scenarioIds = [System.Collections.Generic.List[int]]::new()
    $scenarioBootErrors = 0
    $passCount = 0
    foreach ($line in $Lines) {
        $status = $null
        if ($line -match $scenarioRe) {
            $scenarioKey = $matches[1]
            $status = $matches[2]
            if ($scenarioKey -eq '!') { $scenarioBootErrors++ }
            else { $scenarioIds.Add([int]$scenarioKey) }
        }
        elseif ($line -match $smokeRe)    { $status = $matches[1] }
        elseif ($line -match $bareRe)     { $status = $matches[1] }
        if (-not $status) { continue }   # 結果行でない / ステータス非厳密一致 → 完全に無視
        $items.Add($line)
        if ($status -eq 'PASS') { $passCount++ } else { $fails.Add($line) }
    }
    # foreground trap の痕跡: item1(activate) 単体失敗、または 8 割超の広範囲失敗。
    $item1Fail = [bool](@($fails | Where-Object { $_ -match '\b1 \|' }))
    $wideFail  = ($items.Count -gt 3 -and $fails.Count -ge [int]($items.Count * 0.8))
    return [pscustomobject]@{
        Items       = @($items)
        Fails       = @($fails)
        ParsedCount = $items.Count
        PassCount   = $passCount
        ScenarioIds = @($scenarioIds)
        ScenarioBootErrors = $scenarioBootErrors
        Item1Fail   = $item1Fail
        WideFail    = $wideFail
    }
}

function Test-ScenarioIdSet {
    <#--scenarios の stdout が、全シナリオ ID (1..52 から削除済み 12/16/30/41 を除く) を一度ずつ含むか検証する。#>
    param(
        [Parameter(Mandatory)]$Summary,
        [int]$First = 1,
        [int]$Last = 52,
        [int[]]$Excluded = @(12, 16, 30, 41)
    )
    $ids = @($Summary.ScenarioIds)
    $expected = @($First..$Last | Where-Object { $_ -notin $Excluded })
    if ($ids.Count -ne $expected.Count) {
        return [pscustomobject]@{
            Ok = $false
            Why = "scenario ID count mismatch: got=$($ids.Count) expected=$($expected.Count)"
        }
    }
    $duplicates = @($ids | Group-Object | Where-Object Count -gt 1 | ForEach-Object Name)
    if ($duplicates.Count -gt 0) {
        return [pscustomobject]@{ Ok = $false; Why = "duplicate scenario IDs: $($duplicates -join ',')" }
    }
    $unknown = @($ids | Where-Object { ($_ -lt $First -or $_ -gt $Last) -or ($_ -in $Excluded) } | Sort-Object -Unique)
    if ($unknown.Count -gt 0) {
        return [pscustomobject]@{ Ok = $false; Why = "unknown or retired scenario IDs: $($unknown -join ',')" }
    }
    $seen = [System.Collections.Generic.HashSet[int]]::new()
    foreach ($id in $ids) { [void]$seen.Add([int]$id) }
    $missing = @($expected | Where-Object { -not $seen.Contains([int]$_) })
    if ($missing.Count -gt 0) {
        return [pscustomobject]@{ Ok = $false; Why = "missing scenario IDs: $($missing -join ',')" }
    }
    return [pscustomobject]@{ Ok = $true; Why = '' }
}

function Test-IsScenarioInvocation {
    <#testbench の先頭モードが --scenarios か。後続の --json 等は許容する。#>
    param([string[]]$ArgumentList = @())
    return @($ArgumentList).Count -gt 0 -and $ArgumentList[0] -eq '--scenarios'
}

function Resolve-GateExit {
    <#
    testbench 子プロセスの exit code を、解析した結果行（エビデンス）と突き合わせて
    正規化する。子の exit 0 は「子が成功を主張している」に過ぎず、行エビデンスと
    矛盾する場合は規約（0=pass/1=fail/2=実行不能・エビデンス無し）へ落とす:
      - exit 0 + 解析行ゼロ    -> 2（証拠が無ければ成功を主張できない）
      - exit 0 + FAIL/ERROR 行 -> 1（行が失敗を証明している）
      - 非0 の子 exit          -> そのまま維持（正規化は exit 0 にのみ適用）
    呼び出し側は Write-GateSummary の「前」に呼ぶこと（表示する exit を実際の
    最終 exit と一致させるため）。
    #>
    param(
        [Parameter(Mandatory)]$Summary,
        [int]$Exit = 0
    )
    if ($Exit -ne 0) { return $Exit }
    if ($Summary.ParsedCount -eq 0) { return 2 }
    if ($Summary.Fails.Count -gt 0) { return 1 }
    return 0
}

function Write-GateSummary {
    param(
        [Parameter(Mandatory)]$Summary,
        [int]$Exit = 0,
        [string]$OutFile = ''
    )
    Write-Host ''
    if ($Summary.ParsedCount -eq 0) {
        # パーサーの取りこぼしを成功と誤報しない（fail-closed）。
        Write-Host ("gate: no result lines parsed  exit={0}  out={1}" -f $Exit, $OutFile) -ForegroundColor Yellow
    } else {
        $color = if ($Summary.Fails.Count -eq 0 -and $Exit -eq 0) { 'Green' } else { 'Yellow' }
        Write-Host ("gate: {0}/{1} PASS  exit={2}  out={3}" -f $Summary.PassCount, $Summary.ParsedCount, $Exit, $OutFile) `
            -ForegroundColor $color
    }
    foreach ($f in $Summary.Fails) { Write-Host ("  " + $f) -ForegroundColor Red }
    if ($Summary.Fails.Count -gt 0 -and ($Summary.Item1Fail -or $Summary.WideFail)) {
        Write-Host '  HINT: foreground trap? Headless gates all-FAIL while the user is interacting.' -ForegroundColor Yellow
        Write-Host '        Re-run in a no-interaction window, or use -WaitIdle / -Sandbox.' -ForegroundColor Yellow
    }
}

# ----------------------------------------------------------------------------
# ユーザー無操作待ち（foreground trap の緩和策）
# ----------------------------------------------------------------------------

function Wait-UserIdle {
    <#
    GetLastInputInfo で「ユーザーが N 秒無操作」を検知するまで待つ。

    ヘッドレスゲートはユーザー操作中に全 FAIL する（foreground trap: ユーザー操作中
    は前面ロックが背面プロセスの前面化を拒否し、msctf が TIP を活性化しない）。
    実測では「入力→無操作への遷移直後」(idle 15-30s) が最も通りやすい。深いアイドル
    (200s+) では逆に失敗しやすいため、検知したら即 return する（待ち続けない）。

    戻り値: $true = idle 窓を検知 / $false = TimeoutSec 内に検知できず
    #>
    param(
        [int]$IdleSeconds = 15,
        [int]$TimeoutSec = 900
    )
    if (-not ('GateIdle' -as [type])) {
        Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class GateIdle {
    [StructLayout(LayoutKind.Sequential)]
    public struct LASTINPUTINFO { public uint cbSize; public uint dwTime; }
    [DllImport("user32.dll")]
    public static extern bool GetLastInputInfo(ref LASTINPUTINFO plii);
    public static uint MillisSinceLastInput() {
        var i = new LASTINPUTINFO();
        i.cbSize = (uint)Marshal.SizeOf(typeof(LASTINPUTINFO));
        if (!GetLastInputInfo(ref i)) { return 0; }
        return unchecked((uint)Environment.TickCount - i.dwTime);
    }
}
"@
    }
    Write-Host ("wait-idle: waiting for >= {0}s of no user input (timeout {1}s)..." -f $IdleSeconds, $TimeoutSec)
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ((Get-Date) -lt $deadline) {
        if ([GateIdle]::MillisSinceLastInput() -ge ($IdleSeconds * 1000)) {
            Write-Host 'wait-idle: idle window detected, running the gate now.'
            return $true
        }
        Start-Sleep -Milliseconds 500
    }
    Write-Warn "wait-idle: ${TimeoutSec}s 内に無操作窓を検知できませんでした。"
    return $false
}

# ----------------------------------------------------------------------------
# VC++ 再頒布 DLL（クリーン環境向け app-local 配置）
# ----------------------------------------------------------------------------

function Find-VcRedistDll {
    <#
    VC++ 再頒布 DLL（vcomp140 / vcruntime140 / vcruntime140_1 / msvcp140）の
    最新 x64 コピーを VS インストール配下の Redist から探して FileInfo を返す。

    クリーンPC・Windows Sandbox イメージはこれらを持たず、nospacekey_tip.dll と
    engine は動的 import するため STATUS_DLL_NOT_FOUND (0xC0000135) で即死する。
    app-local 配置は VS 再頒布ライセンスで許可（stage-dist.ps1 §4b/4c と同じ方針）。
    見つからなければ $null（呼び出し側は警告に留めるか失敗にする）。
    #>
    param([Parameter(Mandatory)][string]$Name)
    $candidates = @()
    foreach ($vsRoot in @($env:ProgramFiles, ${env:ProgramFiles(x86)})) {
        if (-not $vsRoot) { continue }
        $r = Join-Path $vsRoot 'Microsoft Visual Studio'
        if (Test-Path $r) {
            $candidates += Get-ChildItem -Path $r -Recurse -Filter $Name -ErrorAction SilentlyContinue |
                Where-Object { $_.FullName -match '\\Redist\\.*\\x64\\' }
        }
    }
    return ($candidates | Sort-Object FullName -Descending | Select-Object -First 1)
}

# ----------------------------------------------------------------------------
# 診断ログ
# ----------------------------------------------------------------------------

function Copy-DiagLogs {
    <#TIP / engine が %TEMP% に吐いた診断ログを last-test-logs へコピー（共有用）。#>
    param([Parameter(Mandatory)][string]$LogCopyDir)
    New-Item -ItemType Directory -Force -Path $LogCopyDir | Out-Null
    foreach ($lp in @((Join-Path $env:TEMP 'nospacekey-tip.log'), (Join-Path $env:TEMP 'nospacekey-engine.log'))) {
        if (Test-Path $lp) {
            Copy-Item $lp (Join-Path $LogCopyDir (Split-Path $lp -Leaf)) -Force -ErrorAction SilentlyContinue
        }
    }
}

function Show-DiagLogs {
    <#
    診断ログの末尾を表示して last-test-logs へコピーする。
    -Hints にスイート固有の確認行（例: SP4 は ev=llm_* 行）を渡して表示を差し込める。
    #>
    param(
        [Parameter(Mandatory)][string]$LogCopyDir,
        [int]$Tail = 40,
        [string[]]$Hints = @()
    )
    Write-Banner '診断ログ（接続/起動/変換のどこで失敗したか）'
    Copy-DiagLogs -LogCopyDir $LogCopyDir
    foreach ($name in @('nospacekey-tip.log', 'nospacekey-engine.log')) {
        $p = Join-Path $env:TEMP $name
        if (Test-Path $p) {
            Write-Host "  [$name] $p" -ForegroundColor Cyan
            Get-Content $p -Tail $Tail -ErrorAction SilentlyContinue | ForEach-Object { Write-Host "    $_" -ForegroundColor Gray }
        } else {
            Write-Host "  [$name] ログ無し: $p" -ForegroundColor DarkGray
        }
    }
    foreach ($h in $Hints) { Write-Host "  $h" -ForegroundColor Green }
    Write-Host "  → コピー先: $LogCopyDir" -ForegroundColor Green
    Write-Host '    失敗時はこの中身（特に TIP ログ）を貼って共有してください。' -ForegroundColor Green
}
