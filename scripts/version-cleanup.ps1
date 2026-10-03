[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$InstallRoot,
    [int]$MaxTrees = 2,
    [int]$DeadlineMs = 3000,
    [int]$CoordinationWaitMs = 60000,
    [switch]$TestFixture,
    [string]$FixtureCurrentTipPath,
    [string[]]$FixtureTaskTargets = @(),
    [string[]]$FixtureActiveBuilds = @(),
    [switch]$FixtureUseRealLeases,
    [string]$FixtureGateName,
    [int]$FixtureHangMs,
    [string]$FixtureWorkerPidPath,
    [string]$FixtureTracePath,
    [string[]]$FixtureDeleteFailureBuilds = @(),
    [string[]]$FixtureCrashAfterMarkerBuilds = @(),
    [string[]]$FixtureCrashAfterClaimBuilds = @(),
    [string[]]$FixtureCrashAfterRetryTransitionRemovedBuilds = @(),
    [string]$FixtureTaskTargetsPath,
    [string]$FixtureBeforeRenameReadyPath,
    [string]$FixtureBeforeRenameContinuePath,
    [string]$FixtureBeforeRetryClaimReadyPath,
    [string]$FixtureBeforeRetryClaimContinuePath,
    [string]$FixtureAfterRetryClaimReadyPath,
    [string]$FixtureAfterRetryClaimContinuePath,
    [string]$FixtureOwnedModelSha256,
    [string]$FixtureValidateOwnedBuild,
    [switch]$RecoverUninstallClaim,
    [switch]$ClaimForUninstall,
    [string]$UninstallBuild,
    [switch]$CommitUninstallTasks,
    [switch]$RecoverUninstallTasks,
    [switch]$FinalizeUninstallTasks,
    [switch]$RecoverInterruptedUninstalls,
    [switch]$ValidateDeletingUninstall,
    [switch]$InitializeTaskTransactionArtifacts,
    [switch]$ValidateFinalUninstallArtifacts,
    [string]$ExpectedRecoverySha256,
    [string]$FixtureUninstallTasksPath,
    [switch]$FixtureStableUninstallJournal,
    [switch]$FixtureStrictTaskXml,
    [switch]$FixtureUseTaskTransactionGate,
    [switch]$FixtureValidatePathAcls,
    [switch]$FixtureValidateAclSddl,
    [string]$FixtureAclSddl,
    [switch]$FixtureValidateTaskArtifactAclSddl,
    [string]$FixtureTaskGateAclSddl,
    [string]$FixtureTaskJournalAclSddl,
    [int]$FixtureHoldTaskGateMs,
    [string]$FixtureTaskGateReadyPath,
    [int]$FixtureUninstallTaskFailureAfter = -1,
    [switch]$FixtureUninstallTaskCrashBeforePublish,
    [int]$FixtureUninstallTaskCrashAfter = -1,
    [switch]$FixtureUninstallTaskCrashAfterDeleting,
    [int]$FixtureUninstallCleanupCrashAfter = -1,
    [switch]$FixtureWaitOwnedMutexOnly,
    [string]$FixtureAbandonedObservedPath,
    [switch]$CleanupWorker,
    [string]$WorkerConfig,
    [switch]$ProbeUninstallTargets,
    [switch]$BeginDeferredUninstall,
    [switch]$AdvanceDeferredUninstall,
    [switch]$CompleteDeferredUninstall,
    [switch]$RollbackDeferredUninstall,
    [switch]$ValidateDeferredUninstallResume,
    [switch]$QueryDeferredUninstallRestart,
    [switch]$RunUninstall,
    [switch]$DeferredConsent,
    [string]$FixturePriorTip,
    [string]$FixtureTipOpsLogPath,
    [switch]$FixtureInnoPostconditionsUnmet,
    [string]$FixtureInnoRegistryPath,
    [string]$FixtureBootId,
    [switch]$FixtureRealReservations,
    [string[]]$FixtureReserveFailurePaths = @(),
    [string[]]$FixtureRetireFailurePaths = @(),
    [string[]]$FixtureDeleteFailurePaths = @(),
    [string[]]$FixtureLeaseProbeFailures = @(),
    [switch]$FixtureRetiredDeleteFails,
    [string]$FixtureDeferredCrashAfter
)

$ErrorActionPreference = 'Stop'
$cleanupState = [pscustomobject]@{
    Started = [Diagnostics.Stopwatch]::StartNew()
    Removed = 0
    Processed = 0
}
$InstallerModelFileName = "ggml-model-Q5_K_M.gguf"
$InstallerModelSha256 = "4de930c06bef8c263aa1aa40684af206db4ce1b96375b3b8ed0ea508e0b14f6c"
$ownedModelSha256 = $InstallerModelSha256
$LifetimeSentinel = '.nospacekey-lifetime'
$QuarantiningSentinel = '.nospacekey-lifetime.quarantining'
$script:InterruptedDeletingBuild = ''
$script:DeferredCrashCounts = @{}
$script:DeferredCollectBlocked = $false
$script:DeferredInnoIncomplete = $false
$script:FixtureTracePath = $FixtureTracePath
$TaskGateSddl = 'O:BAG:SYD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;GR;;;BU)'
$TaskJournalDirectorySddl = 'O:BAG:SYD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;GR;;;BU)'
# NTFS may store GenericRead as file-read, and split an inheritable directory rule into direct and inherit-only ACEs.
$TaskGateAcceptedSddls = @(
    $TaskGateSddl
    'O:BAG:SYD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FR;;;BU)'
)
$TaskJournalDirectoryAcceptedSddls = @(
    $TaskJournalDirectorySddl
    'O:BAG:SYD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;;FR;;;BU)(A;OICIIO;GR;;;BU)'
)
$DeferredJournalMaxBytes = 8388608
$DeferredMaxTrees = 64
$DeferredMaxRounds = 16
$DeferredRetiredPrefix = '.nospacekey-retired-'
$DeferredTipClsid = '{B4B39227-EFF2-41DA-B357-0C3170A57875}'

function Write-FixtureCleanupTrace([string]$Phase, [string]$Detail) {
    if (-not $TestFixture -or [string]::IsNullOrWhiteSpace($script:FixtureTracePath)) { return }
    $temporary = $script:FixtureTracePath + '.' + [Guid]::NewGuid().ToString('N') + '.tmp'
    try {
        $record = [ordered]@{
            Schema = 1
            Phase = $Phase
            Detail = $Detail
            ProcessId = $PID
        } | ConvertTo-Json -Compress
        [IO.File]::WriteAllText($temporary, $record, [Text.UTF8Encoding]::new($false))
        if (Test-Path -LiteralPath $script:FixtureTracePath -PathType Leaf) {
            if (-not [Nospacekey.AtomicFile]::Replace($temporary, $script:FixtureTracePath)) {
                throw 'atomic fixture trace replacement failed'
            }
        } else {
            [IO.File]::Move($temporary, $script:FixtureTracePath)
        }
    } catch {
        Remove-Item -LiteralPath $temporary -Force -ErrorAction SilentlyContinue
    }
}

function Exit-FixtureCleanupFailure([string]$Reason) {
    Write-FixtureCleanupTrace 'explicit-exit' $Reason
    exit 2
}

if (-not ('Nospacekey.LifetimeSentinelClaim' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using Microsoft.Win32.SafeHandles;
using System.Runtime.InteropServices;
namespace Nospacekey {
  public static class PathSafety {
    private const uint FileReadAttributes = 0x00000080;
    private const uint ShareReadWriteDelete = 0x00000007;
    private const uint OpenExisting = 3;
    private const uint BackupSemantics = 0x02000000;
    private const uint OpenReparsePoint = 0x00200000;
    private const uint Directory = 0x00000010;
    private const uint ReparsePoint = 0x00000400;
    [StructLayout(LayoutKind.Sequential)]
    private struct FileAttributeTagInfo { public uint FileAttributes; public uint ReparseTag; }
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    private static extern SafeFileHandle CreateFile(string name, uint access, uint share,
      IntPtr security, uint creation, uint flags, IntPtr template);
    [DllImport("kernel32.dll", SetLastError=true)]
    private static extern bool GetFileInformationByHandleEx(SafeFileHandle file,
      int infoClass, out FileAttributeTagInfo info, uint size);

    // Missing stays distinct so metadata-only uninstall recovery can prove every existing ancestor.
    public static int DirectoryState(string path) {
      var handle = CreateFile(path, FileReadAttributes, ShareReadWriteDelete, IntPtr.Zero,
        OpenExisting, BackupSemantics | OpenReparsePoint, IntPtr.Zero);
      if (handle.IsInvalid) {
        int error = Marshal.GetLastWin32Error();
        handle.Dispose();
        return error == 2 || error == 3 ? 2 : 0;
      }
      try {
        FileAttributeTagInfo info;
        if (!GetFileInformationByHandleEx(handle, 9, out info,
            (uint)Marshal.SizeOf(typeof(FileAttributeTagInfo)))) return 0;
        return (info.FileAttributes & Directory) != 0 &&
          (info.FileAttributes & ReparsePoint) == 0 ? 1 : 0;
      } finally { handle.Dispose(); }
    }
  }

  public static class AtomicFile {
    private const uint ReplaceExisting = 0x00000001;
    private const uint WriteThrough = 0x00000008;
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    private static extern bool MoveFileEx(string existing, string destination, uint flags);
    public static bool Replace(string existing, string destination) {
      return MoveFileEx(existing, destination, ReplaceExisting | WriteThrough);
    }
  }

  public sealed class LifetimeSentinelClaim : IDisposable {
    private const uint GenericRead = 0x80000000;
    private const uint Delete = 0x00010000;
    private const uint ShareReadWriteDelete = 0x00000007;
    private const uint OpenExisting = 3;
    private const uint FileAttributeNormal = 0x00000080;
    [StructLayout(LayoutKind.Sequential)]
    private struct NativeOverlapped {
      public IntPtr Internal;
      public IntPtr InternalHigh;
      public uint Offset;
      public uint OffsetHigh;
      public IntPtr Event;
    }
    [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)]
    private struct FileRenameInfo {
      [MarshalAs(UnmanagedType.Bool)] public bool ReplaceIfExists;
      public IntPtr RootDirectory;
      public uint FileNameLength;
      public char FileName;
    }
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    private static extern SafeFileHandle CreateFile(string name, uint access, uint share,
      IntPtr security, uint creation, uint flags, IntPtr template);
    [DllImport("kernel32.dll", SetLastError=true)]
    private static extern bool LockFileEx(IntPtr file, uint flags, uint reserved,
      uint bytesLow, uint bytesHigh, ref NativeOverlapped overlapped);
    [DllImport("kernel32.dll", SetLastError=true)]
    private static extern bool GetFileSizeEx(SafeFileHandle file, out long size);
    [DllImport("kernel32.dll", SetLastError=true)]
    private static extern bool ReadFile(SafeFileHandle file, byte[] buffer, uint count,
      out uint read, IntPtr overlapped);
    [DllImport("kernel32.dll", SetLastError=true)]
    private static extern bool SetFileInformationByHandle(SafeFileHandle file,
      int infoClass, IntPtr info, uint size);

    private SafeFileHandle handle;
    private LifetimeSentinelClaim(SafeFileHandle handle) { this.handle = handle; }

    public static LifetimeSentinelClaim TryOpen(string path) {
      var handle = CreateFile(path, GenericRead | Delete, ShareReadWriteDelete,
        IntPtr.Zero, OpenExisting, FileAttributeNormal, IntPtr.Zero);
      if (handle.IsInvalid) { handle.Dispose(); return null; }
      byte[] expected = System.Text.Encoding.UTF8.GetBytes("nospacekey version lifetime sentinel\n");
      long size;
      uint read;
      byte[] actual = new byte[expected.Length];
      if (!GetFileSizeEx(handle, out size) || size != expected.Length ||
          !ReadFile(handle, actual, (uint)actual.Length, out read, IntPtr.Zero) || read != actual.Length ||
          !System.Linq.Enumerable.SequenceEqual(actual, expected)) {
        handle.Dispose();
        return null;
      }
      var overlapped = new NativeOverlapped();
      if (!LockFileEx(handle.DangerousGetHandle(), 3, 0, 1, 0, ref overlapped)) {
        handle.Dispose();
        return null;
      }
      return new LifetimeSentinelClaim(handle);
    }

    public bool Rename(string destination) {
      int offset = (int)Marshal.OffsetOf(typeof(FileRenameInfo), "FileName");
      string nativeDestination = destination.StartsWith(@"\??\", StringComparison.Ordinal)
        ? destination : @"\??\" + destination;
      byte[] name = System.Text.Encoding.Unicode.GetBytes(nativeDestination);
      int bufferSize = offset + name.Length + 2;
      IntPtr buffer = Marshal.AllocHGlobal(bufferSize);
      try {
        for (int i = 0; i < bufferSize; i++) { Marshal.WriteByte(buffer, i, 0); }
        Marshal.WriteInt32(buffer, (int)Marshal.OffsetOf(typeof(FileRenameInfo), "FileNameLength"), name.Length);
        Marshal.Copy(name, 0, IntPtr.Add(buffer, offset), name.Length);
        return SetFileInformationByHandle(handle, 3, buffer, (uint)bufferSize);
      } finally { Marshal.FreeHGlobal(buffer); }
    }

    public void Dispose() {
      if (handle != null) { handle.Dispose(); handle = null; }
    }
  }

  // Uninstall-dedicated sentinel access. The regular cleanup claim demands an exclusive
  // byte-range lock, which every in-proc activation also holding a shared lock rejects;
  // that conflation is what forced the old "close all apps" abort. The shared variant
  // coexists with live activations (mirroring Rust open_lifetime_sentinel_path) so the
  // canonical sentinel can be renamed away while those processes keep running, while it
  // still conflicts with the regular cleanup claim's exclusive lock.
  public sealed class LifetimeSentinelUninstallClaim : IDisposable {
    private const uint GenericRead = 0x80000000;
    private const uint Delete = 0x00010000;
    private const uint ShareReadWriteDelete = 0x00000007;
    private const uint OpenExisting = 3;
    private const uint FileAttributeNormal = 0x00000080;
    private const uint OpenReparsePoint = 0x00200000;
    private const uint ReparsePoint = 0x00000400;
    private const uint Directory = 0x00000010;
    private const uint LockFailImmediately = 0x00000001;
    private const uint LockExclusive = 0x00000002;
    private const uint ErrorLockViolation = 33;
    [StructLayout(LayoutKind.Sequential)]
    private struct NativeOverlapped {
      public IntPtr Internal;
      public IntPtr InternalHigh;
      public uint Offset;
      public uint OffsetHigh;
      public IntPtr Event;
    }
    [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)]
    private struct FileRenameInfo {
      [MarshalAs(UnmanagedType.Bool)] public bool ReplaceIfExists;
      public IntPtr RootDirectory;
      public uint FileNameLength;
      public char FileName;
    }
    [StructLayout(LayoutKind.Sequential)]
    private struct FileAttributeTagInfo { public uint FileAttributes; public uint ReparseTag; }
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    private static extern SafeFileHandle CreateFile(string name, uint access, uint share,
      IntPtr security, uint creation, uint flags, IntPtr template);
    [DllImport("kernel32.dll", SetLastError=true)]
    private static extern bool GetFileInformationByHandleEx(SafeFileHandle file,
      int infoClass, out FileAttributeTagInfo info, uint size);
    [DllImport("kernel32.dll", SetLastError=true)]
    private static extern bool GetFileSizeEx(SafeFileHandle file, out long size);
    [DllImport("kernel32.dll", SetLastError=true)]
    private static extern bool ReadFile(SafeFileHandle file, byte[] buffer, uint count,
      out uint read, IntPtr overlapped);
    [DllImport("kernel32.dll", SetLastError=true)]
    private static extern bool LockFileEx(IntPtr file, uint flags, uint reserved,
      uint bytesLow, uint bytesHigh, ref NativeOverlapped overlapped);
    [DllImport("kernel32.dll", SetLastError=true)]
    private static extern bool SetFilePointerEx(SafeFileHandle file, long distance,
      out long position, uint moveMethod);
    [DllImport("kernel32.dll", SetLastError=true)]
    private static extern bool SetFileInformationByHandle(SafeFileHandle file,
      int infoClass, IntPtr info, uint size);

    private SafeFileHandle handle;
    private LifetimeSentinelUninstallClaim(SafeFileHandle handle) { this.handle = handle; }

    // status: 0 claimed, 10 exclusive-lock conflict (in use), 12 validation, 13 I/O failure
    private static LifetimeSentinelUninstallClaim OpenWithLock(string path, bool exclusive, out int status) {
      status = 0;
      var handle = CreateFile(path, GenericRead | Delete, ShareReadWriteDelete, IntPtr.Zero,
        OpenExisting, FileAttributeNormal | OpenReparsePoint, IntPtr.Zero);
      if (handle.IsInvalid) {
        int error = Marshal.GetLastWin32Error();
        handle.Dispose();
        status = (error == 2 || error == 3) ? 12 : 13;
        return null;
      }
      try {
        FileAttributeTagInfo info;
        if (!GetFileInformationByHandleEx(handle, 9, out info,
            (uint)Marshal.SizeOf(typeof(FileAttributeTagInfo)))) {
          status = 13; return null;
        }
        if ((info.FileAttributes & ReparsePoint) != 0 ||
            (info.FileAttributes & Directory) != 0) {
          status = 12; return null;
        }
        // The byte-range lock must be taken before any read: another process holding
        // even a shared lock on the range makes plain reads fail with a lock violation,
        // while reads through the lock-owning handle always succeed.
        var overlapped = new NativeOverlapped();
        uint flags = LockFailImmediately | (exclusive ? LockExclusive : 0u);
        if (!LockFileEx(handle.DangerousGetHandle(), flags, 0, 1, 0, ref overlapped)) {
          int error = Marshal.GetLastWin32Error();
          status = error == ErrorLockViolation ? 10 : 13;
          return null;
        }
        byte[] expected = System.Text.Encoding.UTF8.GetBytes("nospacekey version lifetime sentinel\n");
        long size; uint read;
        byte[] actual = new byte[expected.Length];
        if (!GetFileSizeEx(handle, out size) ||
            !ReadFile(handle, actual, (uint)actual.Length, out read, IntPtr.Zero)) {
          status = 13; return null;
        }
        if (size != expected.Length ||
            read != actual.Length ||
            !System.Linq.Enumerable.SequenceEqual(actual, expected)) {
          status = 12; return null;
        }
        var claim = new LifetimeSentinelUninstallClaim(handle);
        handle = null;
        return claim;
      } catch {
        status = 13;
        return null;
      } finally {
        if (handle != null) { handle.Dispose(); }
      }
    }

    // Reads whole small files whose first bytes may be range-locked by live activations;
    // the read goes through this handle's own shared lock.
    public static byte[] ReadAllBytesSharedLocked(string path, out int error) {
      error = 0;
      var handle = CreateFile(path, GenericRead, ShareReadWriteDelete, IntPtr.Zero,
        OpenExisting, FileAttributeNormal | OpenReparsePoint, IntPtr.Zero);
      if (handle.IsInvalid) {
        error = Marshal.GetLastWin32Error();
        handle.Dispose();
        return null;
      }
      try {
        FileAttributeTagInfo info;
        if (!GetFileInformationByHandleEx(handle, 9, out info,
            (uint)Marshal.SizeOf(typeof(FileAttributeTagInfo))) ||
            (info.FileAttributes & ReparsePoint) != 0 ||
            (info.FileAttributes & Directory) != 0) {
          error = -1;
          return null;
        }
        long size;
        if (!GetFileSizeEx(handle, out size) || size < 0 || size > 1048576) {
          error = -2;
          return null;
        }
        var overlapped = new NativeOverlapped();
        if (!LockFileEx(handle.DangerousGetHandle(), LockFailImmediately, 0, 1, 0, ref overlapped)) {
          error = Marshal.GetLastWin32Error();
          return null;
        }
        byte[] buffer = new byte[size];
        uint read;
        if (!ReadFile(handle, buffer, (uint)buffer.Length, out read, IntPtr.Zero) ||
            read != buffer.Length) {
          error = -3;
          return null;
        }
        return buffer;
      } catch {
        error = -4;
        return null;
      } finally { handle.Dispose(); }
    }

    // Verifies the sentinel bytes through this claim's own handle; the file may be held
    // open by live activations, so plain reads after the rename would fail.
    public bool ContentMatches() {
      byte[] expected = System.Text.Encoding.UTF8.GetBytes("nospacekey version lifetime sentinel\n");
      long position;
      long size; uint read;
      byte[] actual = new byte[expected.Length];
      if (!SetFilePointerEx(handle, 0, out position, 0) || position != 0 ||
          !GetFileSizeEx(handle, out size) || size != expected.Length ||
          !ReadFile(handle, actual, (uint)actual.Length, out read, IntPtr.Zero) ||
          read != actual.Length ||
          !System.Linq.Enumerable.SequenceEqual(actual, expected)) {
        return false;
      }
      return true;
    }

    public static LifetimeSentinelUninstallClaim TryOpenShared(string path, out int status) {
      return OpenWithLock(path, false, out status);
    }

    // 0 = no conflicting lock holder (not in use); other codes as in OpenWithLock.
    public static int ProbeExclusive(string path) {
      int status;
      var claim = OpenWithLock(path, true, out status);
      if (null != claim) { claim.Dispose(); return 0; }
      return status;
    }

    public bool Rename(string destination) {
      int offset = (int)Marshal.OffsetOf(typeof(FileRenameInfo), "FileName");
      string nativeDestination = destination.StartsWith(@"\??\", StringComparison.Ordinal)
        ? destination : @"\??\" + destination;
      byte[] name = System.Text.Encoding.Unicode.GetBytes(nativeDestination);
      int bufferSize = offset + name.Length + 2;
      IntPtr buffer = Marshal.AllocHGlobal(bufferSize);
      try {
        for (int i = 0; i < bufferSize; i++) { Marshal.WriteByte(buffer, i, 0); }
        Marshal.WriteInt32(buffer, (int)Marshal.OffsetOf(typeof(FileRenameInfo), "FileNameLength"), name.Length);
        Marshal.Copy(name, 0, IntPtr.Add(buffer, offset), name.Length);
        return SetFileInformationByHandle(handle, 3, buffer, (uint)bufferSize);
      } finally { Marshal.FreeHGlobal(buffer); }
    }

    public void Dispose() {
      if (handle != null) { handle.Dispose(); handle = null; }
    }
  }

  public static class DeferredDeleteApi {
    private const uint DelayUntilReboot = 0x00000004;
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    private static extern bool MoveFileEx(string existing, string destination, uint flags);
    public static int ScheduleDelete(string path) {
      if (MoveFileEx(path, null, DelayUntilReboot)) return 0;
      return Marshal.GetLastWin32Error();
    }
  }

  public sealed class TaskTransactionClaim : IDisposable {
    private const uint GenericRead = 0x80000000;
    private const uint GenericWrite = 0x40000000;
    private const uint ShareReadWrite = 0x00000003;
    private const uint OpenExisting = 3;
    private const uint OpenReparsePoint = 0x00200000;
    private const uint ReparsePoint = 0x00000400;
    private const uint FileBegin = 0;
    [StructLayout(LayoutKind.Sequential)]
    private struct NativeOverlapped { public IntPtr Internal; public IntPtr InternalHigh; public uint Offset; public uint OffsetHigh; public IntPtr Event; }
    [StructLayout(LayoutKind.Sequential)]
    private struct FileAttributeTagInfo { public uint FileAttributes; public uint ReparseTag; }
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    private static extern SafeFileHandle CreateFile(string name, uint access, uint share,
      IntPtr security, uint creation, uint flags, IntPtr template);
    [DllImport("kernel32.dll", SetLastError=true)]
    private static extern bool LockFileEx(IntPtr file, uint flags, uint reserved,
      uint bytesLow, uint bytesHigh, ref NativeOverlapped overlapped);
    [DllImport("kernel32.dll", SetLastError=true)]
    private static extern bool GetFileSizeEx(SafeFileHandle file, out long size);
    [DllImport("kernel32.dll", SetLastError=true)]
    private static extern bool ReadFile(SafeFileHandle file, byte[] buffer, uint count,
      out uint read, IntPtr overlapped);
    [DllImport("kernel32.dll", SetLastError=true)]
    private static extern bool WriteFile(SafeFileHandle file, byte[] buffer, uint count,
      out uint written, IntPtr overlapped);
    [DllImport("kernel32.dll", SetLastError=true)]
    private static extern bool SetFilePointerEx(SafeFileHandle file, long distance,
      out long position, uint moveMethod);
    [DllImport("kernel32.dll", SetLastError=true)]
    private static extern bool SetEndOfFile(SafeFileHandle file);
    [DllImport("kernel32.dll", SetLastError=true)]
    private static extern bool FlushFileBuffers(SafeFileHandle file);
    [DllImport("kernel32.dll", SetLastError=true)]
    private static extern bool GetFileInformationByHandleEx(SafeFileHandle file,
      int infoClass, out FileAttributeTagInfo info, uint size);

    private SafeFileHandle handle;
    private TaskTransactionClaim(SafeFileHandle handle) { this.handle = handle; }

    private static bool Seek(SafeFileHandle handle, long offset) {
      long position;
      return SetFilePointerEx(handle, offset, out position, FileBegin) && position == offset;
    }

    private static byte[] ReadKnownGate(SafeFileHandle handle) {
      long size;
      if (!GetFileSizeEx(handle, out size) || (size != 43 && size != 44) || !Seek(handle, 0))
        return null;
      byte[] contents = new byte[(int)size];
      uint read;
      if (!ReadFile(handle, contents, (uint)contents.Length, out read, IntPtr.Zero) ||
          read != contents.Length) return null;
      return contents;
    }

    private static bool WaitForLock(SafeFileHandle handle, int timeoutMs) {
      var timer = System.Diagnostics.Stopwatch.StartNew();
      while (true) {
        var overlapped = new NativeOverlapped();
        if (LockFileEx(handle.DangerousGetHandle(), 3, 0, 1, 0, ref overlapped)) return true;
        if (timer.ElapsedMilliseconds >= timeoutMs) return false;
        System.Threading.Thread.Sleep(20);
      }
    }

    public static TaskTransactionClaim TryAcquire(string path, int timeoutMs) {
      var handle = CreateFile(path, GenericRead, ShareReadWrite, IntPtr.Zero,
        OpenExisting, OpenReparsePoint, IntPtr.Zero);
      if (handle.IsInvalid) { handle.Dispose(); return null; }
      byte[] expected = System.Text.Encoding.UTF8.GetBytes("nospacekey scheduled task transaction gate\n");
      long size; uint read; FileAttributeTagInfo info;
      // An exclusive byte-range lock held by another process also fails this
      // handle's plain ReadFile with ERROR_LOCK_VIOLATION, so the content check
      // must not run before WaitForLock; verify it through the locked handle
      // like TryAcquireForInitialization does, or any concurrent holder turns
      // the configured wait into an instant refusal.
      if (!GetFileInformationByHandleEx(handle, 9, out info, (uint)Marshal.SizeOf(typeof(FileAttributeTagInfo))) ||
          (info.FileAttributes & ReparsePoint) != 0 || !GetFileSizeEx(handle, out size) ||
          size != expected.Length || !WaitForLock(handle, timeoutMs)) {
        handle.Dispose(); return null;
      }
      byte[] actual = new byte[expected.Length];
      if (!Seek(handle, 0) || !ReadFile(handle, actual, (uint)actual.Length, out read, IntPtr.Zero) ||
          read != actual.Length || !System.Linq.Enumerable.SequenceEqual(actual, expected)) {
        handle.Dispose(); return null;
      }
      return new TaskTransactionClaim(handle);
    }

    public static TaskTransactionClaim TryAcquireForInitialization(string path, int timeoutMs) {
      var handle = CreateFile(path, GenericRead | GenericWrite, ShareReadWrite, IntPtr.Zero,
        OpenExisting, OpenReparsePoint, IntPtr.Zero);
      if (handle.IsInvalid) { handle.Dispose(); return null; }
      FileAttributeTagInfo info;
      if (!GetFileInformationByHandleEx(handle, 9, out info,
          (uint)Marshal.SizeOf(typeof(FileAttributeTagInfo))) ||
          (info.FileAttributes & ReparsePoint) != 0 || !WaitForLock(handle, timeoutMs)) {
        handle.Dispose(); return null;
      }

      byte[] canonical = System.Text.Encoding.UTF8.GetBytes(
        "nospacekey scheduled task transaction gate\n");
      byte[] legacy = System.Text.Encoding.UTF8.GetBytes(
        "nospacekey scheduled task transaction gate\r\n");
      byte[] interrupted = new byte[canonical.Length + 1];
      Buffer.BlockCopy(canonical, 0, interrupted, 0, canonical.Length);
      interrupted[interrupted.Length - 1] = (byte)'\n';
      byte[] contents = ReadKnownGate(handle);
      if (contents == null) { handle.Dispose(); return null; }
      if (System.Linq.Enumerable.SequenceEqual(contents, canonical))
        return new TaskTransactionClaim(handle);
      bool isLegacy = System.Linq.Enumerable.SequenceEqual(contents, legacy);
      if (!isLegacy && !System.Linq.Enumerable.SequenceEqual(contents, interrupted)) {
        handle.Dispose(); return null;
      }

      if (isLegacy) {
        uint written;
        if (!Seek(handle, 0) ||
            !WriteFile(handle, canonical, (uint)canonical.Length, out written, IntPtr.Zero) ||
            written != canonical.Length || !FlushFileBuffers(handle)) {
          handle.Dispose(); return null;
        }
      }
      if (!Seek(handle, canonical.Length) || !SetEndOfFile(handle) ||
          !FlushFileBuffers(handle)) {
        handle.Dispose(); return null;
      }
      contents = ReadKnownGate(handle);
      if (contents == null || !System.Linq.Enumerable.SequenceEqual(contents, canonical)) {
        handle.Dispose(); return null;
      }
      return new TaskTransactionClaim(handle);
    }

    public void Dispose() { if (handle != null) { handle.Dispose(); handle = null; } }
  }
}
'@
}

function Wait-OwnedMutex([Threading.Mutex]$Mutex, [int]$TimeoutMs) {
    try { return $Mutex.WaitOne($TimeoutMs) }
    catch [Threading.AbandonedMutexException] {
        if ($TestFixture -and -not [string]::IsNullOrWhiteSpace($FixtureAbandonedObservedPath)) {
            [IO.File]::WriteAllText($FixtureAbandonedObservedPath, 'abandoned')
        }
        return $true
    }
}

function Enter-TaskTransaction {
    if ($TestFixture -and -not $FixtureUseTaskTransactionGate) { return $null }
    if ($CoordinationWaitMs -lt 100 -or $CoordinationWaitMs -gt 300000) { return $null }
    if (-not $TestFixture -and -not (Test-TaskTransactionArtifactAcls)) { return $null }
    return [Nospacekey.TaskTransactionClaim]::TryAcquire(
        (Join-Path ([IO.Path]::GetFullPath($InstallRoot)) '.nospacekey-task-transaction'),
        $CoordinationWaitMs)
}

function Get-AllowedAceSignatures($Dacl) {
    $signatures = @()
    foreach ($ace in $Dacl) {
        if ($ace -isnot [Security.AccessControl.CommonAce] -or $ace.IsCallback -or
            $ace.AceQualifier -ne [Security.AccessControl.AceQualifier]::AccessAllowed) {
            throw 'unsupported access rule'
        }
        $mask = [uint32]([int64]$ace.AccessMask -band 0xFFFFFFFFL)
        $signatures += '{0}|{1:X8}|{2}' -f
            $ace.SecurityIdentifier.Value, $mask, [int]$ace.AceFlags
    }
    return $signatures
}

function Test-ExactTaskArtifactAcl($Acl, [string[]]$AcceptedSddls) {
    $descriptor = if ($Acl -is [Security.AccessControl.RawSecurityDescriptor]) { $Acl } else {
        $binary = $Acl.GetSecurityDescriptorBinaryForm()
        [Security.AccessControl.RawSecurityDescriptor]::new($binary, 0)
    }
    $requiredControl = [Security.AccessControl.ControlFlags]::DiscretionaryAclPresent -bor
        [Security.AccessControl.ControlFlags]::DiscretionaryAclProtected
    if (($descriptor.ControlFlags -band $requiredControl) -ne $requiredControl -or
        $null -eq $descriptor.DiscretionaryAcl) {
        return $false
    }
    try {
        $actualAces = Get-AllowedAceSignatures $descriptor.DiscretionaryAcl
    } catch { return $false }
    foreach ($acceptedSddl in $AcceptedSddls) {
        $expected = [Security.AccessControl.RawSecurityDescriptor]::new($acceptedSddl)
        if ($descriptor.Owner -ne $expected.Owner -or
            $descriptor.Group -ne $expected.Group -or
            $descriptor.DiscretionaryAcl.Count -ne $expected.DiscretionaryAcl.Count) {
            continue
        }
        $expectedAces = Get-AllowedAceSignatures $expected.DiscretionaryAcl
        if (@(Compare-Object ($expectedAces | Sort-Object) ($actualAces | Sort-Object)).Count -eq 0) {
            return $true
        }
    }
    return $false
}

function Test-TaskTransactionArtifactAclObjects($GateAcl, $JournalAcl) {
    return (Test-ExactTaskArtifactAcl $GateAcl $TaskGateAcceptedSddls) -and
        (Test-ExactTaskArtifactAcl $JournalAcl $TaskJournalDirectoryAcceptedSddls)
}

function Test-TaskTransactionArtifactAcls {
    try {
        $root = [IO.Path]::GetFullPath($InstallRoot).TrimEnd('\')
        return Test-TaskTransactionArtifactAclObjects `
            (Get-Acl -LiteralPath (Join-Path $root '.nospacekey-task-transaction') -ErrorAction Stop) `
            (Get-Acl -LiteralPath (Join-Path $root '.nospacekey-uninstall') -ErrorAction Stop)
    } catch { return $false }
}

function Get-SidValue($Identity) {
    if ($Identity -is [Security.Principal.SecurityIdentifier]) { return $Identity.Value }
    $reference = if ($Identity -is [Security.Principal.IdentityReference]) { $Identity } else {
        [Security.Principal.NTAccount]::new([string]$Identity)
    }
    return $reference.Translate([Security.Principal.SecurityIdentifier]).Value
}

function Test-SemanticallyProtectedAcl($Acl) {
    try {
        $owner = Get-SidValue $Acl.GetOwner([Security.Principal.SecurityIdentifier])
        $trusted = @(
            'S-1-5-18',
            'S-1-5-32-544',
            'S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464'
        )
        if ($owner -notin $trusted) { return $false }
        $unsafe = [int64]([Security.AccessControl.FileSystemRights]::WriteData) -bor
            [int64]([Security.AccessControl.FileSystemRights]::AppendData) -bor
            [int64]([Security.AccessControl.FileSystemRights]::WriteExtendedAttributes) -bor
            [int64]([Security.AccessControl.FileSystemRights]::WriteAttributes) -bor
            [int64]([Security.AccessControl.FileSystemRights]::DeleteSubdirectoriesAndFiles) -bor
            [int64]([Security.AccessControl.FileSystemRights]::Delete) -bor
            [int64]([Security.AccessControl.FileSystemRights]::ChangePermissions) -bor
            [int64]([Security.AccessControl.FileSystemRights]::TakeOwnership) -bor
            [int64]0x10000000 -bor [int64]0x40000000
        foreach ($rule in @($Acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier]))) {
            if ($rule.AccessControlType -ne [Security.AccessControl.AccessControlType]::Allow) { continue }
            $sid = Get-SidValue $rule.IdentityReference
            if ($sid -ceq 'S-1-3-0' -and
                ($rule.PropagationFlags -band [Security.AccessControl.PropagationFlags]::InheritOnly) -ne 0) {
                continue
            }
            if ($sid -notin $trusted -and (([int64]$rule.FileSystemRights -band $unsafe) -ne 0)) {
                return $false
            }
        }
        return $true
    } catch { return $false }
}

function Test-SemanticallyProtectedDirectory([string]$Path) {
    try { return Test-SemanticallyProtectedAcl (Get-Acl -LiteralPath $Path -ErrorAction Stop) }
    catch { return $false }
}

function Test-SafeDirectoryComponent([string]$Path, [bool]$AllowMissing = $false) {
    $state = [Nospacekey.PathSafety]::DirectoryState($Path)
    if ($state -eq 2) { return $AllowMissing }
    if ($state -ne 1) { return $false }
    if ($TestFixture -and -not $FixtureValidatePathAcls) { return $true }
    return Test-SemanticallyProtectedDirectory $Path
}

function Test-SafeCleanupRootComponents([string]$Root, [bool]$AllowMissingVersions = $false) {
    $fullRoot = [IO.Path]::GetFullPath($Root).TrimEnd('\')
    if (-not (Test-SafeDirectoryComponent $fullRoot)) { return $false }
    return Test-SafeDirectoryComponent (Join-Path $fullRoot 'versions') $AllowMissingVersions
}

function Test-SafeVersionPathComponents([string]$Root, [string]$Build, [bool]$AllowMissingTree = $false) {
    if (-not (Test-SafeBuild $Build)) { return $false }
    $fullRoot = [IO.Path]::GetFullPath($Root).TrimEnd('\')
    if (-not (Test-SafeDirectoryComponent $fullRoot)) { return $false }
    $versions = Join-Path $fullRoot 'versions'
    $versionsState = [Nospacekey.PathSafety]::DirectoryState($versions)
    if ($versionsState -eq 2) { return $AllowMissingTree }
    if ($versionsState -ne 1 -or
        ((-not $TestFixture -or $FixtureValidatePathAcls) -and
        -not (Test-SemanticallyProtectedDirectory $versions))) { return $false }
    return Test-SafeDirectoryComponent (Join-Path $versions $Build) $AllowMissingTree
}

function Initialize-TaskTransactionArtifacts {
    $taskGate = $null
    try {
        $root = [IO.Path]::GetFullPath($InstallRoot).TrimEnd('\')
        $gate = Join-Path $root '.nospacekey-task-transaction'
        $journalRoot = Join-Path $root '.nospacekey-uninstall'
        $gateItem = Get-Item -LiteralPath $gate -Force -ErrorAction Stop
        $journalItem = Get-Item -LiteralPath $journalRoot -Force -ErrorAction Stop
        if ($gateItem.PSIsContainer -or
            (($gateItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) -or
            -not $journalItem.PSIsContainer -or
            (($journalItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) { return $false }
        if ((-not $TestFixture -or $FixtureValidatePathAcls) -and
            (-not (Test-SemanticallyProtectedAcl (Get-Acl -LiteralPath $gate -ErrorAction Stop)) -or
             -not (Test-SemanticallyProtectedAcl (Get-Acl -LiteralPath $journalRoot -ErrorAction Stop)))) {
            return $false
        }
        $taskGate = [Nospacekey.TaskTransactionClaim]::TryAcquireForInitialization(
            $gate, $CoordinationWaitMs)
        if ($null -eq $taskGate) { return $false }
        if ($TestFixture) { return $true }
        $gateAcl = [Security.AccessControl.FileSecurity]::new()
        $gateAcl.SetSecurityDescriptorSddlForm($TaskGateSddl)
        Set-Acl -LiteralPath $gate -AclObject $gateAcl -ErrorAction Stop
        $journalAcl = [Security.AccessControl.DirectorySecurity]::new()
        $journalAcl.SetSecurityDescriptorSddlForm($TaskJournalDirectorySddl)
        Set-Acl -LiteralPath $journalRoot -AclObject $journalAcl -ErrorAction Stop
        return Test-TaskTransactionArtifactAcls
    } catch { return $false }
    finally { if ($null -ne $taskGate) { $taskGate.Dispose() } }
}

function Test-FinalUninstallArtifacts {
    try {
        if ($ExpectedRecoverySha256 -notmatch '^[0-9A-Fa-f]{64}$') { return $false }
        $root = [IO.Path]::GetFullPath($InstallRoot).TrimEnd('\')
        $recovery = Join-Path $root '.nospacekey-uninstall-recovery.ps1'
        $gate = Join-Path $root '.nospacekey-task-transaction'
        $journalRoot = Join-Path $root '.nospacekey-uninstall'
        $versions = Join-Path $root 'versions'
        foreach ($filePath in @($recovery, $gate)) {
            $item = Get-Item -LiteralPath $filePath -Force -ErrorAction Stop
            if ($item.PSIsContainer -or (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) {
                return $false
            }
        }
        if ((Get-FileHash -LiteralPath $recovery -Algorithm SHA256).Hash -cne
            $ExpectedRecoverySha256.ToUpperInvariant() -or
            -not [Linq.Enumerable]::SequenceEqual(
                [byte[]]([IO.File]::ReadAllBytes($gate)),
                [byte[]]([Text.UTF8Encoding]::new($false).GetBytes("nospacekey scheduled task transaction gate`n")))) {
            return $false
        }
        $journal = Get-Item -LiteralPath $journalRoot -Force -ErrorAction Stop
        if (-not $journal.PSIsContainer -or (($journal.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) -or
            @(Get-ChildItem -LiteralPath $journalRoot -Force -ErrorAction Stop).Count -ne 0) { return $false }
        if (Test-Path -LiteralPath $versions) {
            $versionsItem = Get-Item -LiteralPath $versions -Force -ErrorAction Stop
            if (-not $versionsItem.PSIsContainer -or
                (($versionsItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) -or
                @(Get-ChildItem -LiteralPath $versions -Force -ErrorAction Stop).Count -ne 0) { return $false }
        }
        foreach ($entry in @(Get-ChildItem -LiteralPath $root -Force -ErrorAction Stop)) {
            if ($entry.Name -in @('.nospacekey-uninstall-recovery.ps1', '.nospacekey-task-transaction',
                    '.nospacekey-uninstall', 'versions')) { continue }
            if (-not $entry.PSIsContainer -and $entry.Name -match '^unins[0-9]{3}\.(exe|dat|msg)$' -and
                (($entry.Attributes -band [IO.FileAttributes]::ReparsePoint) -eq 0)) { continue }
            return $false
        }
        return $TestFixture -or (Test-TaskTransactionArtifactAcls)
    } catch { return $false }
}

function Test-ReparseFreeTree([string]$Path) {
    $root = Get-Item -LiteralPath $Path -Force
    if (($root.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { return $false }
    foreach ($item in @(Get-ChildItem -LiteralPath $Path -Force -Recurse)) {
        if ($cleanupState.Started.ElapsedMilliseconds -ge $DeadlineMs) { return $false }
        if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { return $false }
    }
    return $true
}

function Wait-FixtureCleanupBarrier([string]$ReadyPath, [string]$ContinuePath, [string]$Value) {
    if (-not $TestFixture -or [string]::IsNullOrWhiteSpace($ReadyPath)) { return $true }
    if ([string]::IsNullOrWhiteSpace($ContinuePath)) { return $false }
    [IO.File]::WriteAllText($ReadyPath, $Value)
    while (-not (Test-Path -LiteralPath $ContinuePath)) {
        if ($cleanupState.Started.ElapsedMilliseconds -ge $DeadlineMs) { return $false }
        Start-Sleep -Milliseconds 10
    }
    return $true
}

function Restore-LifetimeSentinel([string]$Tree) {
    $sentinel = Join-Path $Tree $LifetimeSentinel
    $quarantining = Join-Path $Tree $QuarantiningSentinel
    if (Test-Path -LiteralPath $sentinel) { return $true }
    if (-not (Test-Path -LiteralPath $quarantining -PathType Leaf)) { return $false }
    try {
        [IO.File]::Move($quarantining, $sentinel)
        return $true
    } catch { return $false }
}

function Restore-QuarantiningSentinelAfterDeleteFailure([string]$Tree) {
    if (-not (Test-SafeDirectoryComponent $Tree)) { return }
    $transition = Join-Path $Tree $QuarantiningSentinel
    if (Test-Path -LiteralPath $transition -PathType Leaf) { return }
    $temporary = $transition + '.' + [Guid]::NewGuid().ToString('N') + '.tmp'
    try {
        [IO.File]::WriteAllText($temporary, "nospacekey version lifetime sentinel`n",
            [Text.UTF8Encoding]::new($false))
        [IO.File]::Move($temporary, $transition)
    } catch {
        Remove-Item -LiteralPath $temporary -Force -ErrorAction SilentlyContinue
    }
}

function New-QuarantineDeleteProof([string]$Tree) {
    try {
        $proof = @()
        foreach ($file in @(Get-ChildItem -LiteralPath $Tree -File -Recurse -Force -ErrorAction Stop)) {
            if ($cleanupState.Started.ElapsedMilliseconds -ge $DeadlineMs) { return $null }
            $relative = $file.FullName.Substring($Tree.Length).TrimStart('\').Replace('\', '/')
            if ($relative -in @($LifetimeSentinel, $QuarantiningSentinel)) { continue }
            if ($proof.Count -ge 8192 -or [string]::IsNullOrWhiteSpace($relative) -or
                $relative.Contains('..') -or [IO.Path]::IsPathRooted($relative) -or
                $relative.Contains(':') -or $file.PSIsContainer -or
                (($file.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) { return $null }
            $proof += [ordered]@{
                Path = $relative
                Sha256 = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash
            }
        }
        if ($proof.Count -lt 5 -or $proof.Count -gt 8192) { return $null }
        return $proof
    } catch { return $null }
}

function Test-QuarantineRemainingSubset(
    [string]$Tree,
    $Marker,
    [bool]$RequireComplete = $false,
    [string]$SentinelAlias = $QuarantiningSentinel
) {
    try {
        $proof = @{}
        $observed = 0
        $allowedDirectories = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
        foreach ($entry in @($Marker.Files | ForEach-Object { $_ })) {
            $relative = [string]$entry.Path
            $proof[$relative] = ([string]$entry.Sha256).ToUpperInvariant()
            $parent = [IO.Path]::GetDirectoryName($relative.Replace('/', '\'))
            while (-not [string]::IsNullOrWhiteSpace($parent)) {
                [void]$allowedDirectories.Add($parent.Replace('\', '/'))
                $parent = [IO.Path]::GetDirectoryName($parent)
            }
        }
        foreach ($file in @(Get-ChildItem -LiteralPath $Tree -File -Recurse -Force -ErrorAction Stop)) {
            if ($cleanupState.Started.ElapsedMilliseconds -ge $DeadlineMs) { return $false }
            $relative = $file.FullName.Substring($Tree.Length).TrimStart('\').Replace('\', '/')
            if ($relative -ceq $SentinelAlias) { continue }
            if ($relative -in @($LifetimeSentinel, $QuarantiningSentinel) -or -not $proof.ContainsKey($relative) -or
                (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash -cne $proof[$relative]) {
                return $false
            }
            $observed++
        }
        foreach ($directory in @(Get-ChildItem -LiteralPath $Tree -Directory -Recurse -Force -ErrorAction Stop)) {
            $relative = $directory.FullName.Substring($Tree.Length).TrimStart('\').Replace('\', '/')
            if (-not $allowedDirectories.Contains($relative)) { return $false }
        }
        return -not $RequireComplete -or $observed -eq $proof.Count
    } catch { return $false }
}

function Remove-QuarantineOwnedContent([string]$Tree, [string]$Build) {
    $removed = 0
    $transition = Join-Path $Tree $QuarantiningSentinel
    foreach ($file in @(Get-ChildItem -LiteralPath $Tree -File -Recurse -Force -ErrorAction Stop |
            Where-Object FullName -cne $transition | Sort-Object FullName -Descending)) {
        Remove-Item -LiteralPath $file.FullName -Force -ErrorAction Stop
        $removed++
        if ($TestFixture -and $FixtureDeleteFailureBuilds -contains $Build -and $removed -ge 1) {
            throw 'fixture quarantine deletion failure after owned file removal'
        }
    }
    foreach ($directory in @(Get-ChildItem -LiteralPath $Tree -Directory -Recurse -Force -ErrorAction Stop |
            Sort-Object { $_.FullName.Length } -Descending)) {
        Remove-Item -LiteralPath $directory.FullName -Force -ErrorAction Stop
    }
}

function Open-LifetimeSentinelPathClaim([string]$Path) {
    try {
        $item = Get-Item -LiteralPath $Path -Force -ErrorAction Stop
        if ($item.PSIsContainer -or
            (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) { return $null }
        $claim = [Nospacekey.LifetimeSentinelClaim]::TryOpen($Path)
        return $claim
    } catch { return $null }
}

function Open-LifetimeSentinelClaim([string]$Tree) {
    return Open-LifetimeSentinelPathClaim (Join-Path $Tree $LifetimeSentinel)
}

function Test-SafeBuild([string]$Build) {
    return $Build -match '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$'
}

function Get-VersionTree([string]$Root, [string]$Build) {
    if (-not (Test-SafeBuild $Build)) { return $null }
    return Join-Path ([IO.Path]::GetFullPath($Root).TrimEnd('\')) "versions\$Build"
}

function Recover-VersionTreeUninstallClaim([string]$Root, [string]$Build) {
    if (-not (Test-SafeVersionPathComponents $Root $Build)) { return $false }
    $tree = Get-VersionTree $Root $Build
    if ($null -eq $tree -or -not (Test-ReparseFreeTree $tree)) { return $false }
    $sentinel = Join-Path $tree $LifetimeSentinel
    $transition = $sentinel + '.uninstalling'
    if ((Test-Path -LiteralPath $sentinel) -and (Test-Path -LiteralPath $transition)) { return $false }
    if (Test-Path -LiteralPath $sentinel -PathType Leaf) {
        try {
            return $null -ne (Read-OwnedManifest $tree $Build) -and
                $null -ne (Read-OwnedManifest $tree $Build '' $true)
        } catch { return $false }
    }
    if (-not (Test-Path -LiteralPath $transition -PathType Leaf)) { return $false }
    try { if ($null -eq (Read-OwnedManifest $tree $Build ($LifetimeSentinel + '.uninstalling'))) { return $false } }
    catch { return $false }
    $claim = Open-LifetimeSentinelPathClaim $transition
    if ($null -eq $claim) { return $false }
    try {
        try { if ($null -eq (Read-OwnedManifest $tree $Build ($LifetimeSentinel + '.uninstalling') $true)) { return $false } }
        catch { return $false }
        return $claim.Rename($sentinel)
    } finally { $claim.Dispose() }
}

function Claim-VersionTreeForUninstall([string]$Root, [string]$Build) {
    if (-not (Test-SafeVersionPathComponents $Root $Build)) { return $false }
    $tree = Get-VersionTree $Root $Build
    if ($null -eq $tree) { return $false }
    foreach ($phase in @('pending', 'deleting', 'committed')) {
        $journal = Get-UninstallTaskJournalPath $phase
        if (-not [string]::IsNullOrWhiteSpace($journal) -and (Test-Path -LiteralPath $journal)) { return $false }
    }
    if (-not (Recover-VersionTreeUninstallClaim $Root $Build)) { return $false }
    if (-not (Test-ReparseFreeTree $tree)) { return $false }
    try { if ($null -eq (Read-OwnedManifest $tree $Build)) { return $false } } catch { return $false }
    $claim = Open-LifetimeSentinelClaim $tree
    if ($null -eq $claim) { return $false }
    try {
        try { if ($null -eq (Read-OwnedManifest $tree $Build '' $true)) { return $false } } catch { return $false }
        return $claim.Rename((Join-Path $tree ($LifetimeSentinel + '.uninstalling')))
    } finally { $claim.Dispose() }
}

function Read-OwnedManifest(
    [string]$Tree,
    [string]$ExpectedVersion,
    [string]$LifetimeSentinelAlias = '',
    [bool]$SkipLifetimeSentinelHash = $false
) {
    $path = Join-Path $Tree 'version-manifest.json'
    $item = Get-Item -LiteralPath $path -Force
    if ($item.PSIsContainer -or $item.Length -gt 1048576 -or
        (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) { return $null }
    $record = Get-Content -Raw -LiteralPath $path | ConvertFrom-Json
    if ([int]$record.Schema -ne 1 -or [string]$record.Version -cne $ExpectedVersion) { return $null }
    if (@($record.Files).Count -lt 5 -or @($record.Files).Count -gt 8192) { return $null }
    $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    foreach ($entry in @($record.Files)) {
        if ($cleanupState.Started.ElapsedMilliseconds -ge $DeadlineMs) { return $null }
        $relative = [string]$entry.Path
        if ([string]::IsNullOrWhiteSpace($relative) -or $relative.Contains('..') -or
            [IO.Path]::IsPathRooted($relative) -or $relative.Contains(':') -or
            -not $seen.Add($relative)) { return $null }
        $candidateRelative = if ($relative -ceq $LifetimeSentinel -and
            -not [string]::IsNullOrWhiteSpace($LifetimeSentinelAlias)) { $LifetimeSentinelAlias } else { $relative }
        $candidate = Join-Path $Tree $candidateRelative.Replace('/', '\')
        $file = Get-Item -LiteralPath $candidate -Force
        if ($file.PSIsContainer -or (($file.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) -or
            (-not ($SkipLifetimeSentinelHash -and $relative -ceq $LifetimeSentinel) -and
             (Get-FileHash -LiteralPath $candidate -Algorithm SHA256).Hash -cne [string]$entry.Sha256)) {
            return $null
        }
    }
    foreach ($required in @('.nospacekey-lifetime','nospacekey_tip.dll','NospacekeyEngineHost.exe','NospacekeyConfig.exe','NospacekeyUpdateChecker.exe','version-cleanup.ps1')) {
        if (-not $seen.Contains($required)) { return $null }
    }
    foreach ($file in @(Get-ChildItem -LiteralPath $Tree -File -Recurse -Force)) {
        if ($cleanupState.Started.ElapsedMilliseconds -ge $DeadlineMs) { return $null }
        $relative = $file.FullName.Substring($Tree.Length).TrimStart('\').Replace('\', '/')
        if ($relative -ceq 'version-manifest.json') { continue }
        if (-not [string]::IsNullOrWhiteSpace($LifetimeSentinelAlias) -and
            $relative -ceq $LifetimeSentinelAlias -and $seen.Contains($LifetimeSentinel)) { continue }
        if ($relative -ceq ('models/' + $InstallerModelFileName) -and
            (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash -ceq $ownedModelSha256.ToUpperInvariant()) { continue }
        if (-not $seen.Contains($relative)) { return $null }
    }
    return $record
}

function Get-UninstallTaskJournalPath([string]$Phase = 'pending') {
    if ($Phase -notin @('pending', 'deleting', 'committed')) { return $null }
    if ($TestFixture -and -not $FixtureStableUninstallJournal) {
        if ([string]::IsNullOrWhiteSpace($FixtureUninstallTasksPath)) { return $null }
        $suffix = if ($Phase -ceq 'pending') { '.journal' } else { '.journal.' + $Phase }
        return $FixtureUninstallTasksPath + $suffix
    }
    if (-not (Test-SafeBuild $UninstallBuild)) { return $null }
    return Join-Path ([IO.Path]::GetFullPath($InstallRoot)) `
        ('.nospacekey-uninstall\' + $Phase + '-' + $UninstallBuild + '.json')
}

function Move-UninstallJournalPhase([string]$From, [string]$To) {
    if (($From -ceq 'pending' -and $To -cne 'deleting') -or
        ($From -ceq 'deleting' -and $To -cne 'committed') -or
        $From -notin @('pending', 'deleting') -or $To -notin @('deleting', 'committed')) { return $false }
    $source = Get-UninstallTaskJournalPath $From
    $destination = Get-UninstallTaskJournalPath $To
    if ([string]::IsNullOrWhiteSpace($source) -or [string]::IsNullOrWhiteSpace($destination) -or
        -not (Test-Path -LiteralPath $source -PathType Leaf) -or (Test-Path -LiteralPath $destination) -or
        $null -eq (Read-UninstallTaskJournal $source)) { return $false }
    try {
        Move-Item -LiteralPath $source -Destination $destination -ErrorAction Stop
        return $null -ne (Read-UninstallTaskJournal $destination)
    } catch { return $false }
}

function Get-ProductUpdateTasks {
    return @(Get-ScheduledTask -ErrorAction Stop | Where-Object {
        $_.TaskPath -ceq '\nospacekey\' -and $_.TaskName -like 'UpdateCheck-*'
    })
}

function Get-TaskXmlSha256([string]$Xml) {
    $sha = [Security.Cryptography.SHA256]::Create()
    try { return ([BitConverter]::ToString($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($Xml)))).Replace('-', '').ToLowerInvariant() }
    finally { $sha.Dispose() }
}

function Test-OwnedTaskXml([string]$Name, [string]$Xml, [bool]$RequireOwnedTree = $false) {
    if ($TestFixture -and -not $FixtureStrictTaskXml) {
        return -not [string]::IsNullOrWhiteSpace($Xml)
    }
    if ($Name -notmatch '^UpdateCheck-(S-1(?:-[0-9]+)+)$') { return $false }
    try {
        [xml]$document = $Xml
        $actions = @($document.SelectNodes("//*[local-name()='Actions']"))
        if ($actions.Count -ne 1) { return $false }
        $actionElements = @($actions[0].ChildNodes | Where-Object {
                $_.NodeType -eq [Xml.XmlNodeType]::Element
            })
        $principal = @($document.SelectNodes("//*[local-name()='Principal']/*[local-name()='UserId']"))
        if ($actionElements.Count -ne 1 -or $actionElements[0].LocalName -cne 'Exec' -or
            $principal.Count -ne 1 -or
            [string]$principal[0].InnerText -cne $Matches[1]) { return $false }
        $exec = $actionElements[0]
        $command = @($exec.SelectNodes("./*[local-name()='Command']"))
        $working = @($exec.SelectNodes("./*[local-name()='WorkingDirectory']"))
        $arguments = @($exec.SelectNodes("./*[local-name()='Arguments']"))
        if ($command.Count -ne 1 -or $working.Count -ne 1 -or $arguments.Count -ne 0) { return $false }
        $tree = [IO.Path]::GetFullPath([string]$working[0].InnerText).TrimEnd('\')
        $versions = (Join-Path ([IO.Path]::GetFullPath($InstallRoot).TrimEnd('\')) 'versions') + '\'
        if (-not $tree.StartsWith($versions, [StringComparison]::Ordinal) -or
            [string]$command[0].InnerText -cne (Join-Path $tree 'NospacekeyUpdateChecker.exe')) { return $false }
        $build = $tree.Substring($versions.Length)
        if (-not (Test-SafeBuild $build) -or (Get-VersionTree $InstallRoot $build) -cne $tree) { return $false }
        if (-not $RequireOwnedTree) { return $true }
        if ($build -ceq $UninstallBuild -and
            (Test-Path -LiteralPath (Join-Path $tree ($LifetimeSentinel + '.uninstalling')))) {
            return $null -ne (Read-OwnedManifest $tree $build ($LifetimeSentinel + '.uninstalling') $true)
        }
        return $null -ne (Read-OwnedManifest $tree $build)
    } catch { return $false }
}

function Remove-OwnedUninstallTaskJournalTemps {
    if ($TestFixture) { return }
    try {
        $journal = Get-UninstallTaskJournalPath
        if ([string]::IsNullOrWhiteSpace($journal)) { return }
        $root = Get-Item -LiteralPath (Split-Path -Parent $journal) -Force -ErrorAction Stop
        if (-not $root.PSIsContainer -or
            (($root.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) { return }
        foreach ($item in @(Get-ChildItem -LiteralPath $root.FullName -File -Force -ErrorAction Stop)) {
            if ($item.Name -notmatch '^\.nospacekey-uninstall-tasks\.[0-9a-f]{32}\.tmp$' -or
                $item.Length -gt 1048576 -or
                (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) { continue }
            try {
                $record = [IO.File]::ReadAllText($item.FullName) | ConvertFrom-Json
                $tasks = @($record.Tasks | ForEach-Object { $_ })
                $validTasks = $true
                foreach ($task in $tasks) {
                    if ([string]$task.Name -notmatch '^UpdateCheck-[0-9A-Za-z-]{1,160}$' -or
                        [string]::IsNullOrWhiteSpace([string]$task.Xml)) { $validTasks = $false }
                }
                if ([int]$record.Schema -eq 3 -and [string]$record.Build -ceq $UninstallBuild -and
                    [string]$record.Action -ceq 'Restore' -and
                    $tasks.Count -le 128 -and $validTasks) {
                    Remove-Item -LiteralPath $item.FullName -Force -ErrorAction Stop
                }
            } catch { }
        }
    } catch { }
}

function Get-OwnedUninstallFileProof {
    if ($TestFixture -and -not $FixtureStrictTaskXml) { return @() }
    $tree = Get-VersionTree $InstallRoot $UninstallBuild
    if ($null -eq $tree -or -not (Test-ReparseFreeTree $tree)) { return $null }
    $canonical = Join-Path $tree $LifetimeSentinel
    $transition = $canonical + '.uninstalling'
    if ((Test-Path -LiteralPath $canonical) -eq (Test-Path -LiteralPath $transition)) { return $null }
    $alias = if (Test-Path -LiteralPath $transition) { $LifetimeSentinel + '.uninstalling' } else { '' }
    try {
        $manifest = Read-OwnedManifest $tree $UninstallBuild $alias
        if ($null -eq $manifest) { return $null }
        if ($null -eq (Read-OwnedManifest $tree $UninstallBuild $alias $true)) { return $null }
        $proof = @($manifest.Files | ForEach-Object {
                [ordered]@{ Path = [string]$_.Path; Sha256 = ([string]$_.Sha256).ToUpperInvariant() }
            })
        $manifestPath = Join-Path $tree 'version-manifest.json'
        $proof += [ordered]@{
            Path = 'version-manifest.json'
            Sha256 = (Get-FileHash -LiteralPath $manifestPath -Algorithm SHA256).Hash
        }
        $modelPath = Join-Path $tree ('models\' + $InstallerModelFileName)
        if (Test-Path -LiteralPath $modelPath -PathType Leaf) {
            $modelHash = (Get-FileHash -LiteralPath $modelPath -Algorithm SHA256).Hash
            if ($modelHash -cne $ownedModelSha256.ToUpperInvariant()) { return $null }
            $proof += [ordered]@{ Path = ('models/' + $InstallerModelFileName); Sha256 = $modelHash }
        }
        return @($proof)
    } catch { return $null }
}

function Write-UninstallTaskJournal([object[]]$Snapshots, [string]$FixtureOriginalRaw = '') {
    Remove-OwnedUninstallTaskJournalTemps
    $journal = Get-UninstallTaskJournalPath
    if ([string]::IsNullOrWhiteSpace($journal) -or $Snapshots.Count -gt 128) { return $false }
    foreach ($phase in @('pending', 'deleting', 'committed')) {
        $phasePath = Get-UninstallTaskJournalPath $phase
        if (-not [string]::IsNullOrWhiteSpace($phasePath) -and (Test-Path -LiteralPath $phasePath)) {
            return $false
        }
    }
    $temporaryRoot = if ($TestFixture) { Split-Path -Parent $journal } else {
        Split-Path -Parent $journal
    }
    $temporary = Join-Path $temporaryRoot `
        ('.nospacekey-uninstall-tasks.' + [Guid]::NewGuid().ToString('N') + '.tmp')
    try {
        $ownedFiles = @(Get-OwnedUninstallFileProof)
        if ((-not $TestFixture -or $FixtureStrictTaskXml) -and $ownedFiles.Count -eq 0) {
            throw 'owned uninstall file proof unavailable'
        }
        $journalTasks = @($Snapshots | ForEach-Object {
            $name = [string]$_.Name
            $xml = [string]$_.Xml
            if (-not (Test-OwnedTaskXml $name $xml $true)) { throw 'unsafe scheduled task snapshot' }
            [ordered]@{ Name = $name; Xml = $xml; XmlSha256 = (Get-TaskXmlSha256 $xml) }
        })
        $record = [ordered]@{
            Schema = 3; Build = $UninstallBuild; Action = 'Restore'
            Tasks = $journalTasks; OwnedFiles = $ownedFiles
        }
        if ($TestFixture) { $record.FixtureOriginalRaw = $FixtureOriginalRaw }
        $payload = $record | ConvertTo-Json -Compress -Depth 5
        if ([Text.Encoding]::UTF8.GetByteCount($payload) -gt 1048576) { return $false }
        [IO.File]::WriteAllText($temporary, $payload, [Text.UTF8Encoding]::new($false))
        if ($TestFixture -and $FixtureUninstallTaskCrashBeforePublish) { exit 86 }
        Move-Item -LiteralPath $temporary -Destination $journal -ErrorAction Stop
        if ($null -eq (Read-UninstallTaskJournal)) { return $false }
        return $true
    } catch {
        if (Test-Path -LiteralPath $temporary) {
            Remove-Item -LiteralPath $temporary -Force -ErrorAction SilentlyContinue
        }
        return $false
    }
}

function Read-UninstallTaskJournal([string]$JournalPath = '') {
    $journal = if ([string]::IsNullOrWhiteSpace($JournalPath)) { Get-UninstallTaskJournalPath } else { $JournalPath }
    if ([string]::IsNullOrWhiteSpace($journal)) { return $null }
    $item = Get-Item -LiteralPath $journal -Force -ErrorAction Stop
    if ($item.PSIsContainer -or $item.Length -gt 1048576 -or
        (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) { return $null }
    $record = [IO.File]::ReadAllText($journal) | ConvertFrom-Json
    if ([int]$record.Schema -ne 3 -or [string]$record.Build -cne $UninstallBuild -or
        [string]$record.Action -cne 'Restore') { return $null }
    $tasks = @($record.Tasks | ForEach-Object { $_ })
    if ($tasks.Count -gt 128) { return $null }
    foreach ($task in $tasks) {
        if ([string]$task.Name -notmatch '^UpdateCheck-[0-9A-Za-z-]{1,160}$' -or
            [string]::IsNullOrWhiteSpace([string]$task.Xml) -or
            [string]$task.XmlSha256 -cne (Get-TaskXmlSha256 ([string]$task.Xml)) -or
            -not (Test-OwnedTaskXml ([string]$task.Name) ([string]$task.Xml))) { return $null }
    }
    $ownedFiles = @($record.OwnedFiles | ForEach-Object { $_ })
    if ((-not $TestFixture -or $FixtureStrictTaskXml) -and
        ($ownedFiles.Count -lt 6 -or $ownedFiles.Count -gt 8194)) { return $null }
    $seenFiles = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    foreach ($owned in $ownedFiles) {
        $relative = [string]$owned.Path
        if ([string]::IsNullOrWhiteSpace($relative) -or $relative.Contains('..') -or
            [IO.Path]::IsPathRooted($relative) -or $relative.Contains(':') -or
            -not $seenFiles.Add($relative) -or [string]$owned.Sha256 -notmatch '^[0-9A-Fa-f]{64}$') {
            return $null
        }
    }
    if ((-not $TestFixture -or $FixtureStrictTaskXml) -and
        (-not $seenFiles.Contains($LifetimeSentinel) -or -not $seenFiles.Contains('version-manifest.json'))) {
        return $null
    }
    $fixtureOriginalRaw = if ($TestFixture) { [string]$record.FixtureOriginalRaw } else { '' }
    if ($TestFixture -and [string]::IsNullOrWhiteSpace($fixtureOriginalRaw)) { return $null }
    return [pscustomobject]@{
        Tasks = $tasks; OwnedFiles = $ownedFiles; FixtureOriginalRaw = $fixtureOriginalRaw
    }
}

function Recover-UninstallTasksFromJournal([bool]$KeepJournal = $false) {
    Remove-OwnedUninstallTaskJournalTemps
    $journal = Get-UninstallTaskJournalPath
    if ([string]::IsNullOrWhiteSpace($journal) -or -not (Test-Path -LiteralPath $journal)) { return $true }
    if ($TestFixture) {
        try {
            $record = Read-UninstallTaskJournal
            if ($null -eq $record) { return $false }
            [IO.File]::WriteAllText($FixtureUninstallTasksPath, $record.FixtureOriginalRaw,
                [Text.UTF8Encoding]::new($false))
            if (-not $KeepJournal) { Remove-Item -LiteralPath $journal -Force -ErrorAction Stop }
            return $true
        } catch { return $false }
    }
    try { $record = Read-UninstallTaskJournal } catch { return $false }
    if ($null -eq $record) { return $false }
    $snapshots = @($record.Tasks | ForEach-Object { $_ })
    $failures = [Collections.Generic.List[string]]::new()
    foreach ($snapshot in $snapshots) {
        try {
            $existing = Get-ScheduledTask -TaskName $snapshot.Name -TaskPath '\nospacekey\' -ErrorAction SilentlyContinue
            if ($null -ne $existing) {
                $existingXml = Export-ScheduledTask -TaskName $snapshot.Name -TaskPath '\nospacekey\' -ErrorAction Stop
                if (-not (Test-OwnedTaskXml ([string]$snapshot.Name) $existingXml) -or
                    (Get-TaskXmlSha256 $existingXml) -cne [string]$snapshot.XmlSha256) {
                    throw 'live task is not the transaction snapshot'
                }
            } else {
                Register-ScheduledTask -TaskName $snapshot.Name -TaskPath '\nospacekey\' `
                    -Xml $snapshot.Xml -ErrorAction Stop | Out-Null
            }
        } catch { $failures.Add([string]$snapshot.Name) }
    }
    try {
        foreach ($snapshot in $snapshots) {
            $restoredXml = Export-ScheduledTask -TaskName $snapshot.Name -TaskPath '\nospacekey\' -ErrorAction Stop
            if ((-not (Test-OwnedTaskXml ([string]$snapshot.Name) $restoredXml) -or
                (Get-TaskXmlSha256 $restoredXml) -cne [string]$snapshot.XmlSha256) -and
                -not $failures.Contains($snapshot.Name)) {
                $failures.Add([string]$snapshot.Name)
            }
        }
    } catch { $failures.Add('<verification>') }
    if ($failures.Count -ne 0) {
        [Console]::Error.WriteLine('scheduled task recovery incomplete: ' + ($failures -join ','))
        return $false
    }
    if (-not $KeepJournal) {
        try { Remove-Item -LiteralPath $journal -Force -ErrorAction Stop } catch { return $false }
    }
    return $true
}

function Recover-InterruptedUninstalls {
    if (-not (Test-SafeCleanupRootComponents $InstallRoot $true)) { return $false }
    $transactionRoot = Join-Path ([IO.Path]::GetFullPath($InstallRoot)) '.nospacekey-uninstall'
    try {
        $root = Get-Item -LiteralPath $transactionRoot -Force -ErrorAction Stop
        if (-not $root.PSIsContainer -or
            (($root.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) { return $false }
        $entries = @(Get-ChildItem -LiteralPath $root.FullName -Force -ErrorAction Stop)
        # Completed uninstall can leave only the protected task artifacts after
        # removing versions. With no journal there is no transaction to recover;
        # the schema-3 tree preconditions below apply only to remaining records.
        if ($entries.Count -eq 0) { return $true }
        $temporaries = @($entries | Where-Object {
                    -not $_.PSIsContainer -and $_.Name -match '^\.nospacekey-uninstall-tasks\.[0-9a-f]{32}\.tmp$'
                })
        foreach ($temporary in $temporaries) {
            if ($temporary.Length -gt 1048576 -or
                (($temporary.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) { return $false }
            $temporaryRecord = [IO.File]::ReadAllText($temporary.FullName) | ConvertFrom-Json
            $script:UninstallBuild = [string]$temporaryRecord.Build
            $temporaryTasks = @($temporaryRecord.Tasks | ForEach-Object { $_ })
            if ([int]$temporaryRecord.Schema -ne 3 -or [string]$temporaryRecord.Action -cne 'Restore' -or
                -not (Test-SafeBuild $UninstallBuild) -or $temporaryTasks.Count -gt 128) { return $false }
            foreach ($task in $temporaryTasks) {
                if ([string]$task.XmlSha256 -cne (Get-TaskXmlSha256 ([string]$task.Xml)) -or
                    -not (Test-OwnedTaskXml ([string]$task.Name) ([string]$task.Xml))) { return $false }
            }
        }
        $deferredTemps = @($entries | Where-Object {
                    -not $_.PSIsContainer -and
                    $_.Name -cmatch '^\.nospacekey-deferred-[0-9a-f]{32}\.[0-9a-f]{32}\.tmp$'
                })
        foreach ($deferredTemp in $deferredTemps) {
            if ($deferredTemp.Length -gt $DeferredJournalMaxBytes -or
                (($deferredTemp.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) { return $false }
        }
        # A temp whose JSON is torn is still the residue of an interrupted atomic
        # replace: the shape checks above prove the reserved name, size, and plain
        # file, and the surviving journal (if any) is verified below instead.
        if (@($entries | Where-Object { $_.PSIsContainer -or $_.Name -notmatch `
                    '^(?:\.nospacekey-uninstall-tasks\.[0-9a-f]{32}\.tmp|\.nospacekey-deferred-[0-9a-f]{32}\.[0-9a-f]{32}\.tmp|(?:deferred|(?:pending|deleting|committed))-(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?\.json)$' }).Count -ne 0) { return $false }
        $pending = @($entries | Where-Object { $_.Name.StartsWith('pending-') })
        $deleting = @($entries | Where-Object { $_.Name.StartsWith('deleting-') })
        $committed = @($entries | Where-Object { $_.Name.StartsWith('committed-') })
        $deferred = @($entries | Where-Object { $_.Name.StartsWith('deferred-') })
        if (($pending.Count + $deleting.Count + $committed.Count) -gt 1) { return $false }
        if ($deferred.Count -gt 1) { return $false }
        if ($deferred.Count -eq 1) {
            if (($pending.Count + $deleting.Count + $committed.Count) -ne 0 -or
                $temporaries.Count -ne 0) { return $false }
            $item = $deferred[0]
            $script:UninstallBuild = $item.BaseName.Substring('deferred-'.Length)
            if (-not (Test-SafeBuild $UninstallBuild)) { return $false }
            $deferredRecord = Read-DeferredUninstallJournal $item.FullName
            if ($null -eq $deferredRecord -or
                -not (Remove-DeferredUninstallJournalTemps $deferredTemps $deferredRecord)) { return $false }
            $outcome = Invoke-DeferredUninstallCollect $item.FullName
            if ($outcome -ceq 'ok') { return $true }
            if ($outcome -ceq 'resume') {
                $script:InterruptedDeletingBuild = $script:UninstallBuild
                return $true
            }
            if ($outcome -ceq 'blocked') {
                $script:DeferredCollectBlocked = $true
                return $true
            }
            if ($outcome -ceq 'inno-incomplete') {
                $script:DeferredInnoIncomplete = $true
                return $true
            }
            return $false
        }
        # Preserve schema-3's original path preconditions. Only schema 4 proves that
        # Inno may already have removed the now-empty versions parent.
        if (-not (Test-SafeCleanupRootComponents $InstallRoot)) { return $false }
        if ($deferred.Count -eq 0) {
            # No deferred journal survived, so the temps are the only residue of an
            # interrupted first write; nothing can resume from them and leaving them
            # would abort every future uninstall and block the Rust task gate.
            if (-not (Remove-DeferredUninstallJournalTemps $deferredTemps)) { return $false }
        }
        if ($deleting.Count -eq 1) {
            $item = $deleting[0]
            $script:UninstallBuild = $item.BaseName.Substring('deleting-'.Length)
            if (-not (Test-SafeBuild $UninstallBuild) -or
                -not (Test-DeletingUninstall $UninstallBuild $item.FullName)) { return $false }
            $script:InterruptedDeletingBuild = $UninstallBuild
            return $true
        }
        foreach ($temporary in $temporaries) {
            Remove-Item -LiteralPath $temporary.FullName -Force -ErrorAction Stop
        }
        if ($committed.Count -eq 1) {
            $phase = 'committed'
            $item = $committed[0]
            $script:UninstallBuild = $item.BaseName.Substring(($phase + '-').Length)
            if (-not (Test-SafeBuild $UninstallBuild)) { return $false }
            $record = Read-UninstallTaskJournal $item.FullName
            if ($null -eq $record) { return $false }
            if (-not (Remove-JournalProvenUninstallTree $record)) { return $false }
            Remove-Item -LiteralPath $item.FullName -Force -ErrorAction Stop
            return $true
        }
        if ($pending.Count -eq 0) { return $true }
        $script:UninstallBuild = $pending[0].BaseName.Substring('pending-'.Length)
        if (-not (Test-SafeBuild $UninstallBuild) -or
            (Get-UninstallTaskJournalPath) -cne $pending[0].FullName) { return $false }
        return Recover-PendingUninstall
    } catch { return $false }
}

function Test-DeletingUninstall([string]$ExpectedBuild, [string]$JournalPath = '') {
    if (-not (Test-SafeBuild $ExpectedBuild)) { return $false }
    $script:UninstallBuild = $ExpectedBuild
    if (-not (Test-SafeVersionPathComponents $InstallRoot $ExpectedBuild $true)) { return $false }
    $deleting = if ([string]::IsNullOrWhiteSpace($JournalPath)) {
        Get-UninstallTaskJournalPath 'deleting'
    } else { $JournalPath }
    try {
        if ([string]::IsNullOrWhiteSpace($deleting) -or
            -not (Test-Path -LiteralPath $deleting -PathType Leaf)) { return $false }
        $record = Read-UninstallTaskJournal $deleting
        if ($null -eq $record) { return $false }
        if ($TestFixture) {
            if ([string]::IsNullOrWhiteSpace($FixtureUninstallTasksPath) -or
                -not (Test-Path -LiteralPath $FixtureUninstallTasksPath -PathType Leaf)) { return $false }
            $remainingRaw = [IO.File]::ReadAllText($FixtureUninstallTasksPath)
            $remainingTasks = @($remainingRaw | ConvertFrom-Json | ForEach-Object { $_ })
            if ($remainingTasks.Count -ne 0) {
                return $false
            }
        } elseif (@(Get-ProductUpdateTasks).Count -ne 0) { return $false }
        # Inno calls this immediately before replaying its deletion log. The fixed elevated
        # Program Files layout prevents a non-admin writer from swapping a checked child.
        $tree = Get-VersionTree $InstallRoot $ExpectedBuild
        if ($null -eq $tree -or -not (Test-Path -LiteralPath $tree)) { return $true }
        $treeItem = Get-Item -LiteralPath $tree -Force -ErrorAction Stop
        if (-not $treeItem.PSIsContainer -or
            (($treeItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) -or
            (Test-Path -LiteralPath (Join-Path $tree $LifetimeSentinel))) { return $false }
        $proof = [Collections.Generic.Dictionary[string,string]]::new([StringComparer]::OrdinalIgnoreCase)
        $allowedDirectories = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
        foreach ($owned in @($record.OwnedFiles | ForEach-Object { $_ })) {
            $relative = ([string]$owned.Path).Replace('\', '/')
            $proof[$relative] = ([string]$owned.Sha256).ToUpperInvariant()
            $parent = [IO.Path]::GetDirectoryName($relative.Replace('/', '\'))
            while (-not [string]::IsNullOrWhiteSpace($parent)) {
                [void]$allowedDirectories.Add($parent.Replace('\', '/'))
                $parent = [IO.Path]::GetDirectoryName($parent)
            }
        }
        $transition = Join-Path $tree ($LifetimeSentinel + '.uninstalling')
        if (-not (Test-Path -LiteralPath $transition -PathType Leaf)) { return $false }
        $transitionItem = Get-Item -LiteralPath $transition -Force -ErrorAction Stop
        if ($transitionItem.PSIsContainer -or
            (($transitionItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) { return $false }
        if (-not $proof.ContainsKey($LifetimeSentinel) -or
            (Get-FileHash -LiteralPath $transition -Algorithm SHA256).Hash -cne $proof[$LifetimeSentinel]) {
            return $false
        }
        $directories = [Collections.Generic.Queue[string]]::new()
        $directories.Enqueue($tree)
        while ($directories.Count -ne 0) {
            $directory = $directories.Dequeue()
            foreach ($entry in @(Get-ChildItem -LiteralPath $directory -Force -ErrorAction Stop)) {
                $relative = $entry.FullName.Substring($tree.Length).TrimStart('\').Replace('\', '/')
                if ($entry.PSIsContainer) {
                    if (($entry.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0 -or
                        -not $allowedDirectories.Contains($relative)) { return $false }
                    $directories.Enqueue($entry.FullName)
                    continue
                }
                if (($entry.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { return $false }
                $proofPath = if ($relative -ceq ($LifetimeSentinel + '.uninstalling')) {
                    $LifetimeSentinel
                } else { $relative }
                if (-not $proof.ContainsKey($proofPath) -or
                    (Get-FileHash -LiteralPath $entry.FullName -Algorithm SHA256).Hash -cne $proof[$proofPath]) {
                    return $false
                }
            }
        }
        return $true
    } catch { return $false }
}

function Test-FullTreeMatchesJournalProof([object]$Record) {
    $actual = @(Get-OwnedUninstallFileProof)
    $expected = @($Record.OwnedFiles | ForEach-Object { $_ })
    if ($actual.Count -ne $expected.Count) { return $false }
    $byPath = @{}
    foreach ($entry in $expected) { $byPath[[string]$entry.Path] = ([string]$entry.Sha256).ToUpperInvariant() }
    foreach ($entry in $actual) {
        if (-not $byPath.ContainsKey([string]$entry.Path) -or
            $byPath[[string]$entry.Path] -cne ([string]$entry.Sha256).ToUpperInvariant()) { return $false }
    }
    return $true
}

function Recover-PendingUninstall {
    $pending = Get-UninstallTaskJournalPath
    try {
        if ([string]::IsNullOrWhiteSpace($pending) -or
            -not (Test-Path -LiteralPath $pending -PathType Leaf)) { return $false }
        $record = Read-UninstallTaskJournal $pending
        if ($null -eq $record -or -not (Test-FullTreeMatchesJournalProof $record)) { return $false }
        # A second complete pass immediately before rollback prevents a partially removed tree
        # from regaining its scheduled task and activation sentinel.
        if (-not (Test-FullTreeMatchesJournalProof $record)) { return $false }
        if (-not (Recover-UninstallTasksFromJournal $true)) { return $false }
        if (-not (Recover-VersionTreeUninstallClaim $InstallRoot $UninstallBuild)) { return $false }
        Remove-Item -LiteralPath $pending -Force -ErrorAction Stop
        return $true
    } catch { return $false }
}

function Remove-JournalProvenUninstallTree([object]$Record) {
    if (-not (Test-SafeVersionPathComponents $InstallRoot $UninstallBuild $true)) { return $false }
    $tree = Get-VersionTree $InstallRoot $UninstallBuild
    if ($null -eq $tree) { return $false }
    if (-not (Test-Path -LiteralPath $tree)) { return $true }
    if (-not (Test-ReparseFreeTree $tree)) { return $false }
    $sentinel = Join-Path $tree $LifetimeSentinel
    $transition = $sentinel + '.uninstalling'
    if (Test-Path -LiteralPath $sentinel) { return $false }
    $proof = @{}
    foreach ($entry in @($Record.OwnedFiles | ForEach-Object { $_ })) {
        $proof[[string]$entry.Path] = ([string]$entry.Sha256).ToUpperInvariant()
    }
    $files = @(Get-ChildItem -LiteralPath $tree -File -Recurse -Force -ErrorAction Stop)
    try {
        foreach ($file in $files) {
            $relative = $file.FullName.Substring($tree.Length).TrimStart('\').Replace('\', '/')
            $proofPath = if ($relative -ceq ($LifetimeSentinel + '.uninstalling')) { $LifetimeSentinel } else { $relative }
            if (-not $proof.ContainsKey($proofPath) -or
                (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash -cne $proof[$proofPath]) {
                return $false
            }
        }
        $allowedDirectories = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
        foreach ($path in $proof.Keys) {
            $parent = [IO.Path]::GetDirectoryName(([string]$path).Replace('/', '\'))
            while (-not [string]::IsNullOrWhiteSpace($parent)) {
                [void]$allowedDirectories.Add($parent.Replace('\', '/'))
                $parent = [IO.Path]::GetDirectoryName($parent)
            }
        }
        foreach ($directory in @(Get-ChildItem -LiteralPath $tree -Directory -Recurse -Force -ErrorAction Stop)) {
            $relative = $directory.FullName.Substring($tree.Length).TrimStart('\').Replace('\', '/')
            if (-not $allowedDirectories.Contains($relative)) { return $false }
        }
        $removedProofFiles = 0
        foreach ($file in @($files | Where-Object FullName -cne $transition)) {
            Remove-Item -LiteralPath $file.FullName -Force -ErrorAction Stop
            $removedProofFiles++
            if ($TestFixture -and $FixtureUninstallCleanupCrashAfter -ge 0 -and
                $removedProofFiles -gt $FixtureUninstallCleanupCrashAfter) { exit 86 }
        }
        foreach ($directory in @(Get-ChildItem -LiteralPath $tree -Directory -Recurse -Force -ErrorAction Stop |
                Sort-Object { $_.FullName.Length } -Descending)) {
            if (@(Get-ChildItem -LiteralPath $directory.FullName -Force -ErrorAction Stop).Count -eq 0) {
                Remove-Item -LiteralPath $directory.FullName -Force -ErrorAction Stop
            }
        }
        if (Test-Path -LiteralPath $transition) {
            $claim = Open-LifetimeSentinelPathClaim $transition
            if ($null -eq $claim) { return $false }
            $claim.Dispose()
            Remove-Item -LiteralPath $transition -Force -ErrorAction Stop
        }
        if (@(Get-ChildItem -LiteralPath $tree -Force -ErrorAction Stop).Count -ne 0) { return $false }
        Remove-Item -LiteralPath $tree -Force -ErrorAction Stop
        return $true
    } catch { return $false }
}

function Finalize-UninstallTasks {
    if (-not (Test-SafeVersionPathComponents $InstallRoot $UninstallBuild $true)) { return $false }
    $deleting = Get-UninstallTaskJournalPath 'deleting'
    $committed = Get-UninstallTaskJournalPath 'committed'
    if ([string]::IsNullOrWhiteSpace($deleting) -or [string]::IsNullOrWhiteSpace($committed) -or
        -not (Test-Path -LiteralPath $deleting -PathType Leaf) -or (Test-Path -LiteralPath $committed)) { return $false }
    try {
        $record = Read-UninstallTaskJournal $deleting
        if ($null -eq $record -or -not (Move-UninstallJournalPhase 'deleting' 'committed')) { return $false }
        if (-not (Remove-JournalProvenUninstallTree $record)) { return $false }
        Remove-Item -LiteralPath $committed -Force -ErrorAction Stop
        return $true
    } catch { return $false }
}

function Commit-UninstallTasks {
    if ([string]::IsNullOrWhiteSpace($UninstallBuild)) { return $false }
    $pending = Get-UninstallTaskJournalPath
    if (-not [string]::IsNullOrWhiteSpace($pending) -and (Test-Path -LiteralPath $pending)) {
        [void](Recover-PendingUninstall)
        return $false
    }
    foreach ($phase in @('deleting', 'committed')) {
        $phasePath = Get-UninstallTaskJournalPath $phase
        if (-not [string]::IsNullOrWhiteSpace($phasePath) -and (Test-Path -LiteralPath $phasePath)) {
            return $false
        }
    }
    $deleting = Get-UninstallTaskJournalPath 'deleting'
    if ($TestFixture) {
        if ([string]::IsNullOrWhiteSpace($FixtureUninstallTasksPath)) { return $false }
        $original = [IO.File]::ReadAllText($FixtureUninstallTasksPath)
        $parsedTasks = $original | ConvertFrom-Json
        $tasks = @($parsedTasks | ForEach-Object { $_ })
        try {
            if (-not (Write-UninstallTaskJournal $tasks $original)) { throw 'fixture task journal write failed' }
            for ($index = 0; $index -lt $tasks.Count; $index++) {
                if ($FixtureUninstallTaskFailureAfter -ge 0 -and
                    $index -ge $FixtureUninstallTaskFailureAfter) { throw 'fixture task removal failure' }
                $remaining = @($tasks | Select-Object -Skip ($index + 1))
                [IO.File]::WriteAllText($FixtureUninstallTasksPath,
                    ($remaining | ConvertTo-Json -Compress), [Text.UTF8Encoding]::new($false))
                if ($FixtureUninstallTaskCrashAfter -ge 0 -and
                    $index -ge $FixtureUninstallTaskCrashAfter) { exit 86 }
            }
            [IO.File]::WriteAllText($FixtureUninstallTasksPath, '[]', [Text.UTF8Encoding]::new($false))
            if (-not (Test-FullTreeMatchesJournalProof (Read-UninstallTaskJournal)) -or
                -not (Move-UninstallJournalPhase 'pending' 'deleting')) {
                throw 'fixture deleting phase transition failed'
            }
            if ($FixtureUninstallTaskCrashAfterDeleting) { exit 86 }
            return $true
        } catch {
            if (-not (Test-Path -LiteralPath $deleting)) {
                $pendingAfterFailure = Get-UninstallTaskJournalPath
                if (Test-Path -LiteralPath $pendingAfterFailure) {
                    [void](Recover-PendingUninstall)
                } else {
                    [void](Recover-VersionTreeUninstallClaim $InstallRoot $UninstallBuild)
                }
            }
            return $false
        }
    }
    $snapshots = @()
    try {
        foreach ($task in @(Get-ProductUpdateTasks)) {
            $snapshots += [pscustomobject]@{
                Name = [string]$task.TaskName
                Xml = Export-ScheduledTask -TaskName $task.TaskName -TaskPath '\nospacekey\' -ErrorAction Stop
            }
        }
        if (-not (Write-UninstallTaskJournal $snapshots)) { throw 'task journal write failed' }
        foreach ($snapshot in $snapshots) {
            $liveXml = Export-ScheduledTask -TaskName $snapshot.Name -TaskPath '\nospacekey\' -ErrorAction Stop
            if ((Get-TaskXmlSha256 $liveXml) -cne (Get-TaskXmlSha256 ([string]$snapshot.Xml))) {
                throw 'scheduled task changed after transaction snapshot'
            }
            Unregister-ScheduledTask -TaskName $snapshot.Name -TaskPath '\nospacekey\' -Confirm:$false -ErrorAction Stop
        }
        if (@(Get-ProductUpdateTasks).Count -ne 0) { throw 'update tasks remain' }
        if (-not (Test-FullTreeMatchesJournalProof (Read-UninstallTaskJournal)) -or
            -not (Move-UninstallJournalPhase 'pending' 'deleting')) {
            throw 'deleting phase transition failed'
        }
        return $true
    } catch {
        if (-not (Test-Path -LiteralPath $deleting)) {
            $pendingAfterFailure = Get-UninstallTaskJournalPath
            if (Test-Path -LiteralPath $pendingAfterFailure) {
                [void](Recover-PendingUninstall)
            } else {
                [void](Recover-VersionTreeUninstallClaim $InstallRoot $UninstallBuild)
            }
        }
        return $false
    }
}

function Get-DeferredUninstallJournalPath {
    if (-not (Test-SafeBuild $UninstallBuild)) { return $null }
    return Join-Path ([IO.Path]::GetFullPath($InstallRoot)) ('.nospacekey-uninstall\deferred-' + $UninstallBuild + '.json')
}

function Test-DeferredSafeRelativePath([string]$Path) {
    if ([string]::IsNullOrWhiteSpace($Path) -or $Path.Length -gt 1024 -or
        [IO.Path]::IsPathRooted($Path) -or $Path.Contains(':') -or $Path.Contains('..')) { return $false }
    foreach ($part in @($Path.Replace('\', '/') -split '/')) {
        if ([string]::IsNullOrWhiteSpace($part) -or $part -ceq '.' -or
            $part -cmatch '[<>:"|?*\x00-\x1f]' -or
            $part.StartsWith(' ') -or $part.EndsWith(' ') -or $part.EndsWith('.')) { return $false }
    }
    return $true
}

function Test-DeferredRetiredName([string]$Name) {
    if ([string]::IsNullOrWhiteSpace($Name) -or $Name.Contains('/') -or $Name.Contains('\')) { return $false }
    if ($Name -ceq ($LifetimeSentinel + '.uninstalling')) { return $true }
    return $Name -cmatch ('^' + [regex]::Escape($DeferredRetiredPrefix) + '[0-9a-f]{32}-[0-9]{1,6}$')
}

function Read-DeferredUninstallJournal([string]$JournalPath = '') {
    try {
        $journal = if ([string]::IsNullOrWhiteSpace($JournalPath)) { Get-DeferredUninstallJournalPath } else { $JournalPath }
        if ([string]::IsNullOrWhiteSpace($journal)) { return $null }
        $item = Get-Item -LiteralPath $journal -Force -ErrorAction Stop
        if ($item.PSIsContainer -or $item.Length -gt $DeferredJournalMaxBytes -or
            (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) { return $null }
        $record = [IO.File]::ReadAllText($journal) | ConvertFrom-Json
        if ([int]$record.Schema -ne 4 -or [string]$record.Build -cne $UninstallBuild -or
            [string]$record.TransactionId -cnotmatch '^[0-9a-f]{32}$' -or
            [string]$record.Phase -notin @('pending', 'deleting', 'reboot-pending') -or
            [string]$record.InnoCompletion -notin @('unverified', 'verified') -or
            $null -eq $record.PriorTip -or
            $record.DeferredConsent -isnot [bool] -or $record.RollbackRequested -isnot [bool] -or
            $record.PriorTip.Registered -isnot [bool]) { return $null }
        $priorRegistered = [bool]$record.PriorTip.Registered
        $priorPath = [string]$record.PriorTip.Path
        if ($priorRegistered) {
            $root = [IO.Path]::GetFullPath($InstallRoot).TrimEnd('\')
            $prefix = $root + '\versions\'
            $suffix = '\nospacekey_tip.dll'
            if (-not $priorPath.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase) -or
                -not $priorPath.EndsWith($suffix, [StringComparison]::OrdinalIgnoreCase)) { return $null }
            $priorBuild = $priorPath.Substring($prefix.Length, $priorPath.Length - $prefix.Length - $suffix.Length)
            if (-not (Test-SafeBuild $priorBuild)) { return $null }
        } elseif ($priorPath -ne '') { return $null }
        $tasks = @($record.Tasks | ForEach-Object { $_ })
        if ($tasks.Count -gt 128) { return $null }
        foreach ($task in $tasks) {
            if ([string]$task.Name -notmatch '^UpdateCheck-[0-9A-Za-z-]{1,160}$' -or
                [string]::IsNullOrWhiteSpace([string]$task.Xml) -or
                [string]$task.XmlSha256 -cne (Get-TaskXmlSha256 ([string]$task.Xml)) -or
                -not (Test-OwnedTaskXml ([string]$task.Name) ([string]$task.Xml))) { return $null }
        }
        $trees = @($record.Trees | ForEach-Object { $_ })
        if ($trees.Count -lt 1 -or $trees.Count -gt $DeferredMaxTrees) { return $null }
        $seenTrees = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
        $seenMarkers = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
        foreach ($tree in $trees) {
            $kind = [string]$tree.Kind
            $treePath = [string]$tree.TreePath
            if ($kind -notin @('version', 'quarantine') -or
                -not (Test-DeferredSafeRelativePath $treePath)) { return $null }
            $components = @($treePath.Replace('\', '/') -split '/')
            if ($components.Count -ne 2 -or $components[0] -cne 'versions' -or
                $treePath -cne ('versions\' + $components[1])) { return $null }
            if ($kind -ceq 'version') {
                if (-not (Test-SafeBuild ([string]$tree.Build)) -or
                    $components[1] -cne [string]$tree.Build) { return $null }
            } else {
                if ($components[1] -cnotmatch '^\.cleanup-[0-9a-f]{32}$') { return $null }
            }
            if (-not $seenTrees.Add($treePath)) { return $null }
            $sentinelName = [string]$tree.SentinelName
            if ($sentinelName -ne '' -and
                $sentinelName -notin @($LifetimeSentinel, $QuarantiningSentinel, ($LifetimeSentinel + '.uninstalling'))) { return $null }
            $files = @($tree.Files | ForEach-Object { $_ })
            if ($files.Count -lt 5 -or $files.Count -gt 8192) { return $null }
            $seenFiles = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
            $seenRetired = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
            foreach ($file in $files) {
                $original = ([string]$file.OriginalPath).Replace('\', '/')
                $retired = [string]$file.RetiredPath
                if ([string]$file.OriginalPath -cne $original -or -not (Test-DeferredSafeRelativePath $original) -or
                    [string]$file.Sha256 -cnotmatch '^[0-9A-Fa-f]{64}$' -or
                    [string]$file.RenameState -notin @('none', 'intended', 'renamed') -or
                    -not $seenFiles.Add($original) -or $file.DeleteIntended -isnot [bool] -or
                    $null -eq $file.InitialPath) { return $null }
                if ($retired -ne '' -and -not (Test-DeferredRetiredName $retired)) { return $null }
                if ($retired -ne '' -and -not $seenRetired.Add($retired)) { return $null }
                if ($retired -ne '' -and $retired -cne ($LifetimeSentinel + '.uninstalling') -and
                    -not $retired.StartsWith($DeferredRetiredPrefix + [string]$record.TransactionId + '-', [StringComparison]::Ordinal)) { return $null }
                $initial = [string]$file.InitialPath
                if ($initial -ne '' -and $initial -cne [string]$file.OriginalPath -and
                    -not ($original -ceq $LifetimeSentinel -and $initial -ceq ($LifetimeSentinel + '.uninstalling'))) { return $null }
                if ($kind -ceq 'version' -and $initial -eq '') { return $null }
                if (([string]$file.RenameState -ceq 'none') -and $retired -ne '') { return $null }
                if (([string]$file.RenameState -ne 'none') -and $retired -eq '') { return $null }
            }
            foreach ($retired in $seenRetired) { if ($seenFiles.Contains($retired)) { return $null } }
            $directories = @($tree.OwnedDirectories | ForEach-Object { $_ })
            $seenDirectories = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
            foreach ($directory in $directories) {
                if (([string]$directory).Contains('\') -or -not (Test-DeferredSafeRelativePath ([string]$directory)) -or
                    -not $seenDirectories.Add([string]$directory)) { return $null }
            }
            if ($tree.DeleteIntended -isnot [bool] -or $null -eq $tree.InitialDirectories) { return $null }
            foreach ($directory in @($tree.InitialDirectories)) {
                if (-not $seenDirectories.Contains([string]$directory)) { return $null }
            }
            $marker = $tree.QuarantineMarker
            if ($kind -ceq 'quarantine') {
                if ($null -eq $marker) { return $null }
                $markerPath = [string]$marker.Path
                if ($markerPath -cne ($treePath + '.retry.json') -or
                    [string]$marker.Phase -notin @('ready', 'deleting') -or
                    [string]$marker.Sha256 -cnotmatch '^[0-9A-Fa-f]{64}$' -or
                    -not $seenMarkers.Add($markerPath) -or $marker.DeleteIntended -isnot [bool]) { return $null }
            } elseif ($null -ne $marker) { return $null }
        }
        $rounds = @($record.BootRounds | ForEach-Object { $_ })
        if ($rounds.Count -gt $DeferredMaxRounds) { return $null }
        foreach ($round in $rounds) {
            if ([string]::IsNullOrWhiteSpace([string]$round.BootId) -or
                ([string]$round.BootId).Length -gt 128) { return $null }
            $reservations = @($round.Reservations | ForEach-Object { $_ })
            $seenReservations = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
            foreach ($reservation in $reservations) {
                $path = [string]$reservation.Path
                if (-not (Test-DeferredSafeRelativePath $path) -or
                    -not $path.StartsWith('versions\', [StringComparison]::OrdinalIgnoreCase) -or
                    ([string]$reservation.Kind -cne 'file' -and [string]$reservation.Kind -cne 'directory') -or
                    [string]$reservation.State -notin @('intended', 'issued') -or
                    -not $seenReservations.Add($path)) { return $null }
            }
        }
        $unknowns = @($record.UnknownTargets | ForEach-Object { $_ })
        if ($unknowns.Count -gt 64) { return $null }
        foreach ($unknown in $unknowns) {
            if (-not (Test-DeferredSafeRelativePath ([string]$unknown.Path)) -or
                ([string]$unknown.Reason).Length -gt 256) { return $null }
        }
        $record.Tasks = $tasks
        $record.Trees = $trees
        $record.BootRounds = $rounds
        $record.UnknownTargets = $unknowns
        foreach ($tree in $trees) {
            $tree.Files = @($tree.Files | ForEach-Object { $_ })
            $tree.OwnedDirectories = @($tree.OwnedDirectories | ForEach-Object { $_ })
        }
        foreach ($round in $rounds) { $round.Reservations = @($round.Reservations | ForEach-Object { $_ }) }
        return $record
    } catch { return $null }
}

function Write-DeferredUninstallJournal([object]$Record) {
    $temporary = ''
    try {
        $journal = Get-DeferredUninstallJournalPath
        if ([string]::IsNullOrWhiteSpace($journal)) { return $false }
        $directory = Split-Path -Parent $journal
        if (-not (Test-Path -LiteralPath $directory -PathType Container)) { return $false }
        $payload = $Record | ConvertTo-Json -Compress -Depth 10
        $bytes = [Text.UTF8Encoding]::new($false).GetByteCount($payload)
        if ($bytes -le 0 -or $bytes -gt $DeferredJournalMaxBytes) { return $false }
        $temporary = Join-Path $directory ('.nospacekey-deferred-' + [string]$Record.TransactionId + '.' + [Guid]::NewGuid().ToString('N') + '.tmp')
        $stream = [IO.FileStream]::new($temporary, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
        try {
            $encoded = [Text.UTF8Encoding]::new($false).GetBytes($payload)
            $stream.Write($encoded, 0, $encoded.Length)
            $stream.Flush($true)
        } finally { $stream.Dispose() }
        if (-not [Nospacekey.AtomicFile]::Replace($temporary, $journal)) { return $false }
        $temporary = ''
        $verify = Read-DeferredUninstallJournal $journal
        return ($null -ne $verify -and [IO.File]::ReadAllText($journal) -ceq $payload -and
            [string]$verify.TransactionId -ceq [string]$Record.TransactionId -and
            [string]$verify.Phase -ceq [string]$Record.Phase)
    } catch { return $false }
    finally {
        if ($temporary -ne '' -and (Test-Path -LiteralPath $temporary)) {
            Remove-Item -LiteralPath $temporary -Force -ErrorAction SilentlyContinue
        }
    }
}

function Step-DeferredCrashCounter([string]$Stage) {
    if (-not $TestFixture -or [string]::IsNullOrWhiteSpace($FixtureDeferredCrashAfter)) { return }
    if (-not $script:DeferredCrashCounts.ContainsKey($Stage)) { $script:DeferredCrashCounts[$Stage] = 0 }
    $script:DeferredCrashCounts[$Stage] = $script:DeferredCrashCounts[$Stage] + 1
    $target = $FixtureDeferredCrashAfter
    if ($target -ceq $Stage) { exit 86 }
    if ($target.StartsWith($Stage + ':') -and
        $script:DeferredCrashCounts[$Stage] -ge [int]($target.Substring($Stage.Length + 1))) { exit 86 }
}

function Get-DeferredBootId {
    if ($TestFixture -and -not [string]::IsNullOrWhiteSpace($FixtureBootId)) { return $FixtureBootId }
    try {
        $os = Get-CimInstance -ClassName Win32_OperatingSystem -ErrorAction Stop
        if ($null -eq $os -or $null -eq $os.LastBootUpTime) { return 'unknown' }
        return ([datetime]$os.LastBootUpTime).ToUniversalTime().Ticks.ToString([Globalization.CultureInfo]::InvariantCulture)
    } catch { return 'unknown' }
}

function Add-FixtureTipOp([string]$Operation) {
    if (-not $TestFixture -or [string]::IsNullOrWhiteSpace($FixtureTipOpsLogPath)) { return }
    [IO.File]::AppendAllText($FixtureTipOpsLogPath, $Operation + "`n", [Text.UTF8Encoding]::new($false))
}

function Get-DeferredTipRegistrationPath {
    # Returns the registered InprocServer32 path, or '' when the registration is absent.
    try {
        if ($TestFixture) {
            if ($FixturePriorTip -ceq 'unknown') { throw 'TIP registration could not be read' }
            if ([string]::IsNullOrWhiteSpace($FixtureTipOpsLogPath) -or -not (Test-Path -LiteralPath $FixtureTipOpsLogPath -PathType Leaf)) {
                if ($FixturePriorTip.StartsWith('registered:')) { return $FixturePriorTip.Substring('registered:'.Length) }
                return ''
            }
            # Once the operations log exists it is authoritative: an explicit set-absent
            # never falls back to the initial fixture registration.
            $registered = ''
            foreach ($line in @([IO.File]::ReadAllLines($FixtureTipOpsLogPath))) {
                if ($line.StartsWith('set-registered:', [StringComparison]::Ordinal)) { $registered = $line.Substring('set-registered:'.Length) }
                elseif ($line -ceq 'set-absent') { $registered = '' }
            }
            return $registered
        }
        $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey(
            [Microsoft.Win32.RegistryHive]::LocalMachine,
            [Microsoft.Win32.RegistryView]::Registry64)
        try {
            $key = $base.OpenSubKey('SOFTWARE\Classes\CLSID\' + $DeferredTipClsid + '\InprocServer32', $false)
            if ($null -eq $key) {
                if (-not (Test-DeferredTipAbsent)) { throw 'partial TIP registration remains' }
                return ''
            }
            try {
                $value = $key.GetValue('', $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
                if ($value -isnot [string] -or [string]::IsNullOrWhiteSpace($value)) { throw 'invalid TIP registration value' }
                return $value
            } finally { $key.Dispose() }
        } finally { $base.Dispose() }
    } catch { throw }
}

function Get-PriorTipRegistration {
    # Status: 'absent' (verified unregistered), 'registered' (owned verified target), 'unknown' (refuse).
    try {
        $registeredPath = Get-DeferredTipRegistrationPath
        if ($registeredPath -eq '') {
            if ($TestFixture -and $FixturePriorTip -ceq 'unknown') { return 'unknown' }
            return 'absent'
        }
        $root = [IO.Path]::GetFullPath($InstallRoot).TrimEnd('\')
        $prefix = $root + '\versions\'
        $suffix = '\nospacekey_tip.dll'
        if (-not $registeredPath.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase) -or
            -not $registeredPath.EndsWith($suffix, [StringComparison]::OrdinalIgnoreCase)) { return 'unknown' }
        $build = $registeredPath.Substring($prefix.Length, $registeredPath.Length - $prefix.Length - $suffix.Length)
        if (-not (Test-SafeBuild $build) -or
            -not (Test-Path -LiteralPath $registeredPath -PathType Leaf)) { return 'unknown' }
        $proof = Get-VersionTreeOwnedProof (Join-Path (Join-Path $root 'versions') $build) $build
        if ($null -eq $proof) { return 'unknown' }
        return 'registered'
    } catch { return 'unknown' }
}

function Invoke-DeferredTipUnregister([string]$TipPath) {
    try {
        if ($TestFixture) {
            Add-FixtureTipOp 'set-absent'
            return $true
        }
        $process = Start-Process -FilePath (Join-Path $env:SystemRoot 'System32\regsvr32.exe') `
            -ArgumentList @('/u', '/s', ('"{0}"' -f $TipPath)) -Wait -PassThru -WindowStyle Hidden
        if ($process.ExitCode -ne 0) { return $false }
        return (Test-DeferredTipAbsent)
    } catch { return $false }
}

function Invoke-DeferredTipRegister([string]$TipPath) {
    try {
        if ($TestFixture) {
            Add-FixtureTipOp ('set-registered:' + $TipPath)
            return $true
        }
        $process = Start-Process -FilePath (Join-Path $env:SystemRoot 'System32\regsvr32.exe') `
            -ArgumentList @('/s', ('"{0}"' -f $TipPath)) -Wait -PassThru -WindowStyle Hidden
        if ($process.ExitCode -ne 0) { return $false }
        $current = Get-DeferredTipRegistrationPath
        return ($current -ne '' -and $current.Equals($TipPath, [StringComparison]::OrdinalIgnoreCase))
    } catch { return $false }
}

function Get-DeferredTaskSnapshots {
    if ($TestFixture) {
        if ([string]::IsNullOrWhiteSpace($FixtureUninstallTasksPath) -or
            -not (Test-Path -LiteralPath $FixtureUninstallTasksPath -PathType Leaf)) { return @() }
        $parsed = Get-Content -Raw -LiteralPath $FixtureUninstallTasksPath | ConvertFrom-Json
        return @($parsed | ForEach-Object { $_ })
    }
    $snapshots = @()
    foreach ($task in @(Get-ProductUpdateTasks)) {
        $snapshots += [pscustomobject]@{
            Name = [string]$task.TaskName
            Xml = Export-ScheduledTask -TaskName $task.TaskName -TaskPath '\nospacekey\' -ErrorAction Stop
        }
    }
    return @($snapshots)
}

function Test-DeferredTasksPresent {
    try {
        if ($TestFixture) {
            if ([string]::IsNullOrWhiteSpace($FixtureUninstallTasksPath) -or
                -not (Test-Path -LiteralPath $FixtureUninstallTasksPath -PathType Leaf)) { return $false }
            return @((Get-Content -Raw -LiteralPath $FixtureUninstallTasksPath | ConvertFrom-Json |
                    ForEach-Object { $_ })).Count -gt 0
        }
        return @(Get-ProductUpdateTasks).Count -gt 0
    } catch { return $true }
}

function Remove-DeferredTasks([object[]]$Snapshots = @()) {
    # Only journal-proven snapshots are deleted, each re-verified against the live
    # task before unregistration; anything else bearing the UpdateCheck- prefix is
    # left behind so the caller fails and rolls back. Schema 3 never unregistered
    # re-enumerated tasks unconditionally and this path must not regress it.
    try {
        if ($TestFixture) {
            if ([string]::IsNullOrWhiteSpace($FixtureUninstallTasksPath)) { return $true }
            $live = @(Get-DeferredTaskSnapshots)
            $index = 0
            foreach ($snapshot in @($Snapshots)) {
                if ($FixtureUninstallTaskFailureAfter -ge 0 -and $index -ge $FixtureUninstallTaskFailureAfter) { return $false }
                $entry = @($live | Where-Object { ([string]$_.Name) -ceq ([string]$snapshot.Name) })
                if ($entry.Count -eq 1) {
                    if ((Get-TaskXmlSha256 ([string]$entry[0].Xml)) -cne [string]$snapshot.XmlSha256) { return $false }
                    $live = @($live | Where-Object { ([string]$_.Name) -cne ([string]$snapshot.Name) })
                    [IO.File]::WriteAllText($FixtureUninstallTasksPath,
                        (ConvertTo-Json -InputObject $live -Compress -Depth 4))
                }
                $index++
            }
            return -not (Test-DeferredTasksPresent)
        }
        foreach ($snapshot in @($Snapshots)) {
            $existing = Get-ScheduledTask -TaskName ([string]$snapshot.Name) -TaskPath '\nospacekey\' -ErrorAction SilentlyContinue
            if ($null -eq $existing) { continue }
            $liveXml = Export-ScheduledTask -TaskName ([string]$snapshot.Name) -TaskPath '\nospacekey\' -ErrorAction Stop
            if (-not (Test-OwnedTaskXml ([string]$snapshot.Name) $liveXml) -or
                (Get-TaskXmlSha256 $liveXml) -cne [string]$snapshot.XmlSha256) { return $false }
            Unregister-ScheduledTask -TaskName ([string]$snapshot.Name) -TaskPath '\nospacekey\' `
                -Confirm:$false -ErrorAction Stop
        }
        return (@(Get-ProductUpdateTasks).Count -eq 0)
    } catch { return $false }
}

function Restore-DeferredTasks([object[]]$Snapshots) {
    try {
        if ($TestFixture) {
            if ([string]::IsNullOrWhiteSpace($FixtureUninstallTasksPath)) { return $true }
            [IO.File]::WriteAllText($FixtureUninstallTasksPath,
                (@($Snapshots | ConvertTo-Json -Compress -Depth 4)), [Text.UTF8Encoding]::new($false))
            return (Test-DeferredTasksPresent)
        }
        $failures = [Collections.Generic.List[string]]::new()
        foreach ($snapshot in @($Snapshots)) {
            try {
                $existing = Get-ScheduledTask -TaskName ([string]$snapshot.Name) -TaskPath '\nospacekey\' -ErrorAction SilentlyContinue
                if ($null -ne $existing) {
                    $existingXml = Export-ScheduledTask -TaskName ([string]$snapshot.Name) -TaskPath '\nospacekey\' -ErrorAction Stop
                    if (-not (Test-OwnedTaskXml ([string]$snapshot.Name) $existingXml) -or
                        (Get-TaskXmlSha256 $existingXml) -cne [string]$snapshot.XmlSha256) { throw 'live task differs' }
                } else {
                    Register-ScheduledTask -TaskName ([string]$snapshot.Name) -TaskPath '\nospacekey\' `
                        -Xml ([string]$snapshot.Xml) -ErrorAction Stop | Out-Null
                }
            } catch { $failures.Add([string]$snapshot.Name) }
        }
        if ($failures.Count -ne 0) { return $false }
        foreach ($snapshot in @($Snapshots)) {
            $restoredXml = Export-ScheduledTask -TaskName ([string]$snapshot.Name) -TaskPath '\nospacekey\' -ErrorAction Stop
            if ((Get-TaskXmlSha256 $restoredXml) -cne [string]$snapshot.XmlSha256) { return $false }
        }
        return $true
    } catch { return $false }
}

function Invoke-DeferredSentinelRename([string]$Tree, [string]$FromName, [string]$ToName) {
    # Caller must persist the rename intent first. Returns 0 ok / 10 in use / 12 validation / 13 I/O.
    $from = Join-Path $Tree $FromName
    $to = Join-Path $Tree $ToName
    if (Test-Path -LiteralPath $to) { return 12 }
    $status = 0
    $claim = [Nospacekey.LifetimeSentinelUninstallClaim]::TryOpenShared($from, [ref]$status)
    if ($null -eq $claim) { return $status }
    try {
        if (-not $claim.Rename($to)) { return 13 }
        # The retired name stays held by live activations; only this lock-owning handle
        # can still read the bytes for the post-rename identity verification.
        if (-not $claim.ContentMatches()) { return 12 }
    } finally { $claim.Dispose() }
    if (Test-Path -LiteralPath $from) { return 13 }
    if (-not (Test-Path -LiteralPath $to -PathType Leaf)) { return 13 }
    return 0
}

function Invoke-DeferredReserveApi([string]$AbsolutePath) {
    if ($TestFixture -and -not $FixtureRealReservations) {
        foreach ($failure in @($FixtureReserveFailurePaths)) {
            if ($failure.Equals($AbsolutePath, [StringComparison]::OrdinalIgnoreCase)) { return 87 }
        }
        return 0
    }
    return [Nospacekey.DeferredDeleteApi]::ScheduleDelete($AbsolutePath)
}

function Invoke-DeferredFileDelete([string]$AbsolutePath) {
    if ($TestFixture) {
        foreach ($failure in @($FixtureDeleteFailurePaths)) {
            if ($failure.Equals($AbsolutePath, [StringComparison]::OrdinalIgnoreCase)) {
                throw 'fixture deferred deletion failure'
            }
        }
        if ($FixtureRetiredDeleteFails -and
            (Split-Path -Leaf $AbsolutePath).StartsWith($DeferredRetiredPrefix, [StringComparison]::Ordinal)) {
            throw 'fixture retired deletion failure'
        }
    }
    Remove-Item -LiteralPath $AbsolutePath -Force -ErrorAction Stop
}

function Get-VersionTreeOwnedProof([string]$Tree, [string]$Build) {
    try {
        if (-not (Test-SafeBuild $Build) -or -not (Test-SafeDirectoryComponent $Tree) -or
            -not (Test-ReparseFreeTree $Tree)) { return $null }
        $canonical = Join-Path $Tree $LifetimeSentinel
        $uninstalling = $canonical + '.uninstalling'
        $alias = ''
        if (Test-Path -LiteralPath $uninstalling -PathType Leaf) {
            if (Test-Path -LiteralPath $canonical) { return $null }
            $alias = $LifetimeSentinel + '.uninstalling'
        } elseif (-not (Test-Path -LiteralPath $canonical -PathType Leaf)) { return $null }
        # ContentMatches-style hash is computed through a shared lock: live activations
        # range-lock the sentinel, so a plain manifest hash read would fail outright.
        $sentinelBytes = Get-DeferredLockedFileBytes (Join-Path $Tree $(if ($alias -ne '') { $alias } else { $LifetimeSentinel }))
        if ($null -eq $sentinelBytes) { return $null }
        $sentinelHash = ConvertTo-DeferredSha256FromBytes $sentinelBytes
        try { $manifest = Read-OwnedManifest $Tree $Build $alias $true } catch { return $null }
        if ($null -eq $manifest) { return $null }
        $files = @()
        foreach ($entry in @($manifest.Files | ForEach-Object { $_ })) {
            $path = [string]$entry.Path
            $fileRecord = [ordered]@{
                OriginalPath = $path; RetiredPath = ''
                Sha256 = ([string]$entry.Sha256).ToUpperInvariant(); RenameState = 'none'
            }
            if ($path -ceq $LifetimeSentinel) {
                # The observed bytes must match the manifest's recorded sentinel hash;
                # schema 3 proves the retired alias the same way instead of trusting
                # the reserved name alone.
                if ($sentinelHash -cne ([string]$entry.Sha256).ToUpperInvariant()) { return $null }
                $fileRecord.Sha256 = $sentinelHash
                if ($alias -ne '') {
                    $fileRecord.RetiredPath = $alias
                    $fileRecord.RenameState = 'renamed'
                }
            }
            $files += $fileRecord
        }
        $manifestPath = Join-Path $Tree 'version-manifest.json'
        $files += [ordered]@{
            OriginalPath = 'version-manifest.json'; RetiredPath = ''
            Sha256 = (Get-FileHash -LiteralPath $manifestPath -Algorithm SHA256).Hash; RenameState = 'none'
        }
        $modelPath = Join-Path $Tree ('models\' + $InstallerModelFileName)
        if (Test-Path -LiteralPath $modelPath -PathType Leaf) {
            $modelHash = (Get-FileHash -LiteralPath $modelPath -Algorithm SHA256).Hash
            if ($modelHash -cne $ownedModelSha256.ToUpperInvariant()) { return $null }
            $files += [ordered]@{
                OriginalPath = ('models/' + $InstallerModelFileName); RetiredPath = ''
                Sha256 = $modelHash; RenameState = 'none'
            }
        }
        $directories = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
        foreach ($file in $files) {
            $parent = [IO.Path]::GetDirectoryName(([string]$file.OriginalPath).Replace('/', '\'))
            while (-not [string]::IsNullOrWhiteSpace($parent)) {
                [void]$directories.Add($parent.Replace('\', '/'))
                $parent = [IO.Path]::GetDirectoryName($parent)
            }
        }
        foreach ($directory in @(Get-ChildItem -LiteralPath $Tree -Directory -Recurse -Force -ErrorAction Stop)) {
            if ($cleanupState.Started.ElapsedMilliseconds -ge $DeadlineMs) { return $null }
            $relative = $directory.FullName.Substring($Tree.Length).TrimStart('\').Replace('\', '/')
            if (-not $directories.Add($relative)) { }
        }
        return [pscustomobject]@{
            SentinelName = if ($alias -ne '') { $alias } else { $LifetimeSentinel }
            Files = @($files)
            Directories = @($directories)
        }
    } catch { return $null }
}

function Get-QuarantineOwnedProof([string]$Tree) {
    try {
        if (-not (Test-SafeDirectoryComponent $Tree) -or -not (Test-ReparseFreeTree $Tree)) { return $null }
        $marker = Read-QuarantineMarker $Tree
        if ($null -eq $marker) { return $null }
        $canonical = Join-Path $Tree $LifetimeSentinel
        $quarantining = Join-Path $Tree $QuarantiningSentinel
        $sentinelName = ''
        if (Test-Path -LiteralPath $canonical -PathType Leaf) {
            if (Test-Path -LiteralPath $quarantining -PathType Leaf) { return $null }
            $sentinelName = $LifetimeSentinel
        } elseif (Test-Path -LiteralPath $quarantining -PathType Leaf) {
            $sentinelName = $QuarantiningSentinel
        }
        $complete = [string]$marker.Phase -ceq 'ready'
        if ($complete -and $sentinelName -ceq '') { return $null }
        $alias = if ($sentinelName -ceq '') { $QuarantiningSentinel } else { $sentinelName }
        if ($complete) {
            $manifestPath = Join-Path $Tree 'version-manifest.json'
            if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf) -or
                (Get-FileHash -LiteralPath $manifestPath -Algorithm SHA256).Hash -cne
                    [string]$marker.ManifestSha256) { return $null }
            $manifestAlias = if ($sentinelName -ceq $QuarantiningSentinel) { $QuarantiningSentinel } else { '' }
            try { if ($null -eq (Read-OwnedManifest $Tree ([string]$marker.Build) $manifestAlias $true)) { return $null } }
            catch { return $null }
            if (-not (Test-QuarantineRemainingSubset $Tree $marker $true $alias)) { return $null }
        } else {
            if (-not (Test-QuarantineRemainingSubset $Tree $marker $false $alias)) { return $null }
        }
        $files = @()
        foreach ($entry in @($marker.Files | ForEach-Object { $_ })) {
            $files += [ordered]@{
                OriginalPath = [string]$entry.Path; RetiredPath = ''
                Sha256 = ([string]$entry.Sha256).ToUpperInvariant(); RenameState = 'none'
            }
        }
        if ($sentinelName -ne '') {
            $sentinelBytes = Get-DeferredLockedFileBytes (Join-Path $Tree $sentinelName)
            if ($null -eq $sentinelBytes) { return $null }
            # The quarantine marker never records the sentinel, so the canonical
            # content is the only anchor; anything else riding the reserved name
            # must be rejected rather than adopted as owned.
            $expectedSentinelHash = ConvertTo-DeferredSha256FromBytes (
                [Text.UTF8Encoding]::new($false).GetBytes("nospacekey version lifetime sentinel`n"))
            $observedSentinelHash = ConvertTo-DeferredSha256FromBytes $sentinelBytes
            if ($observedSentinelHash -cne $expectedSentinelHash) { return $null }
            $files += [ordered]@{
                OriginalPath = $sentinelName; RetiredPath = ''
                Sha256 = $observedSentinelHash
                RenameState = 'none'
            }
        }
        $directories = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
        foreach ($file in $files) {
            $parent = [IO.Path]::GetDirectoryName(([string]$file.OriginalPath).Replace('/', '\'))
            while (-not [string]::IsNullOrWhiteSpace($parent)) {
                [void]$directories.Add($parent.Replace('\', '/'))
                $parent = [IO.Path]::GetDirectoryName($parent)
            }
        }
        return [pscustomobject]@{
            SentinelName = $sentinelName
            Files = @($files)
            Directories = @($directories)
            MarkerPhase = [string]$marker.Phase
        }
    } catch { return $null }
}

function Get-DeferredUninstallTargets {
    # Null = hard validation failure. Unknown residue is reported, never deleted.
    try {
        $root = [IO.Path]::GetFullPath($InstallRoot).TrimEnd('\')
        if (-not (Test-SafeCleanupRootComponents $root)) { return $null }
        $transactionRoot = Join-Path $root '.nospacekey-uninstall'
        if (-not (Test-Path -LiteralPath $transactionRoot -PathType Container)) { return $null }
        foreach ($phase in @('pending', 'deleting', 'committed')) {
            $phaseJournals = @(Get-ChildItem -LiteralPath $transactionRoot -File -Force |
                Where-Object { $_.Name -cmatch ('^' + $phase + '-.+\.json$') })
            if ($phaseJournals.Count -ne 0) { return $null }
        }
        $versions = Join-Path $root 'versions'
        if (-not (Test-Path -LiteralPath $versions -PathType Container)) { return $null }
        $trees = @()
        $unknown = @()
        $hasCurrent = $false
        $claimedMarkers = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
        foreach ($entry in @(Get-ChildItem -LiteralPath $versions -Force -ErrorAction Stop)) {
            if (-not $entry.PSIsContainer) { continue }
            if ($entry.Name -like '.cleanup-*') {
                if ($entry.Name -cnotmatch '^\.cleanup-[0-9a-f]{32}$') {
                    $unknown += [ordered]@{ Path = ('versions\' + $entry.Name); Reason = 'unknown quarantine name' }
                    continue
                }
                $proof = Get-QuarantineOwnedProof $entry.FullName
                if ($null -eq $proof) {
                    $unknown += [ordered]@{ Path = ('versions\' + $entry.Name); Reason = 'quarantine ownership unprovable' }
                    continue
                }
                $markerPath = $entry.FullName + '.retry.json'
                $markerHash = (Get-FileHash -LiteralPath $markerPath -Algorithm SHA256).Hash
                [void]$claimedMarkers.Add($markerPath.ToLowerInvariant())
                $trees += [ordered]@{
                    Kind = 'quarantine'; Build = ''
                    TreePath = ('versions\' + $entry.Name)
                    SentinelName = [string]$proof.SentinelName
                    Files = @($proof.Files | ForEach-Object { $_ })
                    OwnedDirectories = @($proof.Directories | ForEach-Object { $_ })
                    QuarantineMarker = [ordered]@{
                        Path = ('versions\' + $entry.Name + '.retry.json')
                        Sha256 = $markerHash
                        Phase = [string]$proof.MarkerPhase
                    }
                }
                continue
            }
            if (-not (Test-SafeBuild $entry.Name)) {
                $unknown += [ordered]@{ Path = ('versions\' + $entry.Name); Reason = 'unsafe version name' }
                continue
            }
            $proof = Get-VersionTreeOwnedProof $entry.FullName $entry.Name
            if ($null -eq $proof) {
                # Inno's appended deletion log may still contain this version. Leaving it
                # behind would hand unproven files back to standard uninstall deletion.
                return $null
            }
            if ($entry.Name -ceq $UninstallBuild) { $hasCurrent = $true }
            $trees += [ordered]@{
                Kind = 'version'; Build = $entry.Name
                TreePath = ('versions\' + $entry.Name)
                SentinelName = [string]$proof.SentinelName
                Files = @($proof.Files | ForEach-Object { $_ })
                OwnedDirectories = @($proof.Directories | ForEach-Object { $_ })
                QuarantineMarker = $null
            }
        }
        if (-not $hasCurrent) { return $null }
        foreach ($entry in @(Get-ChildItem -LiteralPath $versions -File -Force -ErrorAction Stop)) {
            if ($entry.Name -cmatch '^\.cleanup-[0-9a-f]{32}\.retry\.json$' -and
                $claimedMarkers.Contains($entry.FullName.ToLowerInvariant())) { continue }
            $unknown += [ordered]@{ Path = ('versions\' + $entry.Name); Reason = 'unrecognized versions entry' }
        }
        return [pscustomobject]@{ Trees = @($trees); Unknown = @($unknown) }
    } catch { return $null }
}

function Test-DeferredTargetInUse([object]$Target) {
    # 'free', 'inuse', or 'fail' (distinguished from in-use per the dedicated claim contract).
    try {
        $root = [IO.Path]::GetFullPath($InstallRoot).TrimEnd('\')
        $build = [string]$Target.Build
        if ($build -eq '') {
            $marker = Read-QuarantineMarker (Join-Path $root ([string]$Target.TreePath))
            if ($null -eq $marker) { return 'fail' }
            $build = [string]$marker.Build
        }
        $leaseState = Test-BuildLeaseInactive $build
        if ($leaseState -ceq 'active') { return 'inuse' }
        if ($leaseState -ceq 'fail') { return 'fail' }
        $sentinelName = [string]$Target.SentinelName
        if ($sentinelName -ne '') {
            $sentinelPath = Join-Path (Join-Path $root ([string]$Target.TreePath)) $sentinelName
            if (-not (Test-Path -LiteralPath $sentinelPath)) {
                $sentinel = @($Target.Files | Where-Object { $_.OriginalPath -ceq $LifetimeSentinel -and $_.RetiredPath -ne '' })
                if ($sentinel.Count -eq 1) {
                    $sentinelPath = Join-Path (Join-Path $root ([string]$Target.TreePath)) ([string]$sentinel[0].RetiredPath)
                }
            }
            if (Test-Path -LiteralPath $sentinelPath -PathType Leaf) {
                $status = [Nospacekey.LifetimeSentinelUninstallClaim]::ProbeExclusive($sentinelPath)
                if ($status -eq 10) { return 'inuse' }
                if ($status -ne 0) { return 'fail' }
            }
        }
        return 'free'
    } catch { return 'fail' }
}

function Ensure-DeferredRound([object]$Journal) {
    $bootId = Get-DeferredBootId
    # An unidentifiable boot must never own a reservation round: the boundary
    # check below can then never prove a boot change, permanently blocking the
    # journal. Failing here keeps deleting and lets the next run retry.
    if ($bootId -ceq 'unknown') { return $null }
    $rounds = @($Journal.BootRounds | ForEach-Object { $_ })
    if ($rounds.Count -gt 0 -and [string]$rounds[$rounds.Count - 1].BootId -ceq $bootId) {
        return $rounds[$rounds.Count - 1]
    }
    if ($rounds.Count -ge $DeferredMaxRounds) { return $null }
    $round = [ordered]@{ BootId = $bootId; Reservations = @() }
    $Journal.BootRounds = @($rounds + $round)
    if (-not (Write-DeferredUninstallJournal $Journal)) { return $null }
    return $round
}

function Invoke-DeferredReservePath([object]$Journal, [string]$Root, [string]$RelativePath, [string]$Kind) {
    # Returns 0 issued (or re-issued) / 1 failed. Intent is persisted before the API call.
    if (-not [bool]$Journal.DeferredConsent) { return 10 }
    $absolute = Join-Path $Root $RelativePath
    $round = Ensure-DeferredRound $Journal
    if ($null -eq $round) { return 1 }
    $entry = $null
    foreach ($reservation in @($round.Reservations | ForEach-Object { $_ })) {
        if ([string]$reservation.Path -ceq $RelativePath) { $entry = $reservation; break }
    }
    if ($null -eq $entry) {
        $entry = [ordered]@{ Path = $RelativePath; Kind = $Kind; State = 'intended' }
        $round.Reservations = @(@($round.Reservations | ForEach-Object { $_ }) + $entry)
        if (-not (Write-DeferredUninstallJournal $Journal)) { return 1 }
        Step-DeferredCrashCounter 'reserve-intent'
    }
    $code = Invoke-DeferredReserveApi $absolute
    Step-DeferredCrashCounter 'reserve'
    if ($code -ne 0) { return 1 }
    if ([string]$entry.State -cne 'issued') {
        $entry.State = 'issued'
        if (-not (Write-DeferredUninstallJournal $Journal)) { return 1 }
    }
    return 0
}

function Get-DeferredLockedFileBytes([string]$Path) {
    $error = 0
    $bytes = [Nospacekey.LifetimeSentinelUninstallClaim]::ReadAllBytesSharedLocked($Path, [ref]$error)
    if ($null -eq $bytes) { return $null }
    return [byte[]]$bytes
}

function ConvertTo-DeferredSha256FromBytes([byte[]]$Bytes) {
    $sha = [Security.Cryptography.SHA256]::Create()
    try {
        return ([BitConverter]::ToString($sha.ComputeHash($Bytes))).Replace('-', '').ToUpperInvariant()
    } finally { $sha.Dispose() }
}

function Get-DeferredFileHashWithRetry([string]$Path) {
    # Get-FileHash wraps its failure in a PowerShell RuntimeException whose
    # InnerException chain no longer carries the Win32 code, so the raw open
    # helper is the only path that still reports 2/3 gone and 303 delete-pending.
    for ($attempt = 0; $attempt -lt 3; $attempt++) {
        $error = 0
        $bytes = [Nospacekey.LifetimeSentinelUninstallClaim]::ReadAllBytesSharedLocked($Path, [ref]$error)
        if ($null -ne $bytes) { return (ConvertTo-DeferredSha256FromBytes $bytes) }
        if ($error -eq 303) { return 'delete-pending' }
        if ($error -eq 2 -or $error -eq 3) { return '' }
        if ($error -eq -2 -or $error -eq 32) {
            # Above the small-file cap, or held with a narrower share mask: only the
            # sentinel is ever range-locked and it is far below the cap, so a plain
            # hash read is the right fallback for ordinary files.
            try { return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash } catch { }
            Start-Sleep -Milliseconds 100
            continue
        }
        Start-Sleep -Milliseconds 100
    }
    return ''
}

function Test-DeferredRegistryKeyExists([string]$View, [string]$Path) {
    if ($TestFixture) {
        if ([string]::IsNullOrWhiteSpace($FixtureInnoRegistryPath)) { return $false }
        $keys = @([IO.File]::ReadAllText($FixtureInnoRegistryPath) | ConvertFrom-Json | ForEach-Object { $_ })
        if ($keys -contains 'read-error') { throw 'registry read failure' }
        return $keys -contains ($View + '|' + $Path)
    }
    $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::LocalMachine,
        [Microsoft.Win32.RegistryView]::$View)
    try {
        $key = $base.OpenSubKey($Path, $false)
        if ($null -eq $key) { return $false }
        $key.Dispose()
        return $true
    } finally { $base.Dispose() }
}

function Test-DeferredTipAbsent {
    try {
        if ($TestFixture -and (Get-DeferredTipRegistrationPath) -ne '') { return $false }
        foreach ($view in @('Registry64', 'Registry32')) {
            foreach ($path in @(('SOFTWARE\Classes\CLSID\' + $DeferredTipClsid), ('SOFTWARE\Microsoft\CTF\TIP\' + $DeferredTipClsid))) {
                if (Test-DeferredRegistryKeyExists $view $path) { return $false }
            }
        }
        return $true
    } catch { return $false }
}

function Test-DeferredInnoPostconditions([bool]$AllowMissingInstallRoot = $false) {
    try {
        if ($TestFixture -and $FixtureInnoPostconditionsUnmet) { return $false }
        if (-not (Test-DeferredTipAbsent) -or (Test-DeferredTasksPresent)) { return $false }
        foreach ($view in @('Registry64', 'Registry32')) {
            foreach ($path in @('SOFTWARE\Classes\nospacekey', 'SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\nospacekey_is1')) {
                if (Test-DeferredRegistryKeyExists $view $path) { return $false }
            }
        }
        $group = if ($TestFixture) { Join-Path $InstallRoot 'shortcuts' }
            else { Join-Path ([Environment]::GetFolderPath('CommonPrograms')) 'nospacekey' }
        if (Test-Path -LiteralPath $group) { return $false }
        if (Test-Path -LiteralPath $InstallRoot) {
            foreach ($entry in @(Get-ChildItem -LiteralPath $InstallRoot -Force -ErrorAction Stop)) {
                if ($entry.Name -match '^unins[0-9]{3}\.(exe|dat|msg)$') { return $false }
            }
        } elseif (-not $AllowMissingInstallRoot) {
            return $false
        }
        return $true
    } catch { return $false }
}

function Invoke-DeferredLimitedRecovery {
    # Next-installer collection may clear provably owned Inno leftovers (protocol key and
    # shortcut group). Anything not exactly product-owned is left untouched.
    if ($TestFixture) { return $false }
    try {
        $root = [IO.Path]::GetFullPath($InstallRoot).TrimEnd('\')
        $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey(
            [Microsoft.Win32.RegistryHive]::LocalMachine,
            [Microsoft.Win32.RegistryView]::Registry64)
        try {
            $protocol = $base.OpenSubKey('SOFTWARE\Classes\nospacekey', $true)
            if ($null -ne $protocol) {
                $owned = $true
                $description = [string]$protocol.GetValue('', $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
                if (-not $description.StartsWith('URL:nospacekey', [StringComparison]::Ordinal)) { $owned = $false }
                $commandKey = $protocol.OpenSubKey('shell\open\command', $false)
                $iconKey = $protocol.OpenSubKey('DefaultIcon', $false)
                if ($null -eq $commandKey -or $null -eq $iconKey) { $owned = $false }
                if ($owned) {
                    $command = [string]$commandKey.GetValue('', $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
                    $iconValue = [string]$iconKey.GetValue('', $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
                    $match = [regex]::Match($command, '^"([^"]+)" "%1"$')
                    if (-not $match.Success -or -not (Test-DeferredProductExecutable $root $match.Groups[1].Value) -or
                        -not $iconValue.Equals($match.Groups[1].Value + ',0', [StringComparison]::OrdinalIgnoreCase)) { $owned = $false }
                }
                if ($null -ne $commandKey) { $commandKey.Dispose() }
                if ($null -ne $iconKey) { $iconKey.Dispose() }
                $protocol.Dispose()
                if ($owned) { $base.DeleteSubKeyTree('SOFTWARE\Classes\nospacekey', $false) }
            }
        } finally { $base.Dispose() }
        $group = Join-Path ([Environment]::GetFolderPath('CommonPrograms')) 'nospacekey'
        if (Test-Path -LiteralPath $group) {
            if (((Get-Item -LiteralPath $group -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { return $false }
            $ownedShortcuts = $true
            $shell = New-Object -ComObject WScript.Shell
            try {
                foreach ($item in @(Get-ChildItem -LiteralPath $group -Force)) {
                    if ($item.PSIsContainer -or $item.Extension -cne '.lnk') { $ownedShortcuts = $false; break }
                    $link = $shell.CreateShortcut($item.FullName)
                    $target = [string]$link.TargetPath
                    if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0 -or
                        -not (Test-DeferredProductExecutable $root $target)) { $ownedShortcuts = $false }
                    if (-not $ownedShortcuts) { break }
                }
            } finally { [void][Runtime.InteropServices.Marshal]::ReleaseComObject($shell) }
            if ($ownedShortcuts) {
                # Each observed link is verified above; never recurse through a group.
                foreach ($item in @(Get-ChildItem -LiteralPath $group -Force)) { Remove-Item -LiteralPath $item.FullName -Force -ErrorAction Stop }
                [IO.Directory]::Delete($group)
            }
        }
        return $true
    } catch { return $false }
}

function Test-DeferredProductExecutable([string]$Root, [string]$Path) {
    $prefix = $Root.TrimEnd('\') + '\versions\'
    if (-not $Path.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) { return $false }
    $parts = @($Path.Substring($prefix.Length) -split '\\')
    return $parts.Count -eq 2 -and (Test-SafeBuild $parts[0]) -and $parts[1] -ceq 'NospacekeyConfig.exe'
}

function Test-DeferredReservationBoundary([object]$Journal) {
    $rounds = @($Journal.BootRounds)
    if ($rounds.Count -eq 0) { return $true }
    $boot = Get-DeferredBootId
    # Unknown identities can never prove that an earlier reservation was consumed.
    return $boot -cne 'unknown' -and
        @($rounds | Where-Object { $_.BootId -ceq 'unknown' -or $_.BootId -ceq $boot }).Count -eq 0
}

function Test-DeferredTreeState([object]$TreeRecord, [bool]$RequireInitial, [bool]$OriginalsAbsent = $false) {
    try {
        $root = [IO.Path]::GetFullPath($InstallRoot).TrimEnd('\')
        if (-not (Test-SafeCleanupRootComponents $root (-not $RequireInitial))) { return $false }
        $treeAbs = Join-Path $root ([string]$TreeRecord.TreePath)
        if (-not (Test-SafeDirectoryComponent $treeAbs $true)) { return $false }
        $exists = Test-Path -LiteralPath $treeAbs -PathType Container
        if (-not $exists -and ($RequireInitial -or -not [bool]$TreeRecord.DeleteIntended)) { return $false }
        if ($exists -and -not (Test-ReparseFreeTree $treeAbs)) { return $false }
        $allowed = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
        foreach ($entry in @($TreeRecord.Files)) {
            $original = ([string]$entry.OriginalPath).Replace('\', '/')
            $retired = [string]$entry.RetiredPath
            $originalPath = Join-Path $treeAbs $original.Replace('/', '\')
            $hasOriginal = Test-Path -LiteralPath $originalPath
            $hasRetired = $retired -ne '' -and (Test-Path -LiteralPath (Join-Path $treeAbs $retired))
            if ($hasOriginal -and $hasRetired) { return $false }
            if ($OriginalsAbsent -and [string]$TreeRecord.Kind -ceq 'version' -and $hasOriginal) { return $false }
            if (-not $RequireInitial -and $hasOriginal -and
                (([string]$TreeRecord.Kind -ceq 'version' -and $original -ceq $LifetimeSentinel) -or
                 [string]$entry.RenameState -ceq 'renamed')) { return $false }
            $actual = if ($hasOriginal) { $original } elseif ($hasRetired) { $retired } else { '' }
            if ([string]$entry.InitialPath -eq '' -and $actual -ne '') { return $false }
            if ($RequireInitial) {
                if (([string]$entry.InitialPath -eq '') -ne ($actual -eq '')) { return $false }
            } elseif ($actual -eq '' -and [string]$entry.InitialPath -ne '' -and -not [bool]$entry.DeleteIntended) {
                return $false
            }
            if ($actual -ne '') {
                [void]$allowed.Add($actual)
                $hash = Get-DeferredFileHashWithRetry (Join-Path $treeAbs $actual.Replace('/', '\'))
                if ($hash -ceq 'delete-pending' -and -not $RequireInitial -and [bool]$entry.DeleteIntended) { continue }
                if ($hash -cne [string]$entry.Sha256) { return $false }
            }
        }
        $directories = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
        if ($exists) {
            foreach ($file in @(Get-ChildItem -LiteralPath $treeAbs -File -Recurse -Force)) {
                $relative = $file.FullName.Substring($treeAbs.Length).TrimStart('\').Replace('\', '/')
                if (-not $allowed.Contains($relative)) { return $false }
            }
            foreach ($directory in @(Get-ChildItem -LiteralPath $treeAbs -Directory -Recurse -Force)) {
                $relative = $directory.FullName.Substring($treeAbs.Length).TrimStart('\').Replace('\', '/')
                if (@($TreeRecord.OwnedDirectories) -notcontains $relative) { return $false }
                [void]$directories.Add($relative)
            }
        }
        if ($RequireInitial -and -not $directories.SetEquals([string[]]@($TreeRecord.InitialDirectories))) { return $false }
        $marker = $TreeRecord.QuarantineMarker
        if ($null -ne $marker) {
            $path = Join-Path $root ([string]$marker.Path)
            if (Test-Path -LiteralPath $path) {
                $item = Get-Item -LiteralPath $path -Force
                if ($item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0 -or
                    (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash -cne [string]$marker.Sha256) { return $false }
            } elseif ($RequireInitial -or -not [bool]$marker.DeleteIntended) { return $false }
        }
        return $true
    } catch { return $false }
}

function Test-DeferredJournalTrees([object]$Journal, [bool]$RequireInitial, [bool]$OriginalsAbsent = $false) {
    foreach ($tree in @($Journal.Trees)) {
        if (-not (Test-DeferredTreeState $tree $RequireInitial $OriginalsAbsent)) { return $false }
    }
    return $true
}

function Invoke-DeferredProcessTree([string]$Root, [object]$TreeRecord, [object]$Journal) {
    # Forward-only file/directory processing for one journal tree. 0 ok / 12 validation / 13 I/O.
    try {
        if (-not (Test-DeferredTreeState $TreeRecord $false)) { return 12 }
        # This entire, validated tree is already in the forward-only deleting phase.
        # Persist its file deletion intents together before touching any payload.
        # Rewriting and revalidating the full journal once per dictionary file makes
        # real installations with thousands of files take minutes to uninstall.
        $intentChanged = -not [bool]$TreeRecord.DeleteIntended
        $TreeRecord.DeleteIntended = $true
        foreach ($entry in @($TreeRecord.Files)) {
            if (-not [bool]$entry.DeleteIntended) {
                $entry.DeleteIntended = $true
                $intentChanged = $true
            }
        }
        if ($intentChanged -and -not (Write-DeferredUninstallJournal $Journal)) { return 13 }
        $treeAbs = Join-Path $Root ([string]$TreeRecord.TreePath)
        if (-not (Test-Path -LiteralPath $treeAbs)) {
            $marker = $TreeRecord.QuarantineMarker
            if ($null -ne $marker) {
                $markerAbs = Join-Path $Root ([string]$marker.Path)
                if (Test-Path -LiteralPath $markerAbs -PathType Leaf) {
                    $marker.DeleteIntended = $true
                    if (-not (Write-DeferredUninstallJournal $Journal)) { return 13 }
                    try { Remove-Item -LiteralPath $markerAbs -Force -ErrorAction Stop } catch {
                        if (-not $Journal.DeferredConsent) { return 10 }
                        if ((Invoke-DeferredReservePath $Journal $Root ([string]$marker.Path) 'file') -ne 0) { return 13 }
                    }
                }
            }
            return 0
        }
        $proof = @{}
        $retiredToOriginal = @{}
        foreach ($fileEntry in @($TreeRecord.Files | ForEach-Object { $_ })) {
            $proof[([string]$fileEntry.OriginalPath).ToLowerInvariant()] = $fileEntry
            $retiredName = [string]$fileEntry.RetiredPath
            if ($retiredName -ne '') {
                $retiredToOriginal[$retiredName.ToLowerInvariant()] = ([string]$fileEntry.OriginalPath).ToLowerInvariant()
            }
        }
        $presentAtOriginal = @{}
        $presentAtRetired = @{}
        foreach ($file in @(Get-ChildItem -LiteralPath $treeAbs -File -Recurse -Force -ErrorAction Stop)) {
            if ($cleanupState.Started.ElapsedMilliseconds -ge $DeadlineMs) { return 13 }
            $relative = $file.FullName.Substring($treeAbs.Length).TrimStart('\').Replace('\', '/')
            $relativeKey = $relative.ToLowerInvariant()
            if ($proof.ContainsKey($relativeKey)) { $presentAtOriginal[$relativeKey] = $file.FullName }
            elseif ($relative.IndexOf('/') -lt 0 -and $retiredToOriginal.ContainsKey($relativeKey)) {
                $presentAtRetired[$retiredToOriginal[$relativeKey]] = $file.FullName
            } else { return 12 }
        }
        $allowedDirectories = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
        foreach ($fileKey in $proof.Keys) {
            $parent = [IO.Path]::GetDirectoryName($fileKey.Replace('/', '\'))
            while (-not [string]::IsNullOrWhiteSpace($parent)) {
                [void]$allowedDirectories.Add($parent.Replace('\', '/'))
                $parent = [IO.Path]::GetDirectoryName($parent)
            }
        }
        foreach ($directory in @($TreeRecord.OwnedDirectories | ForEach-Object { $_ })) {
            [void]$allowedDirectories.Add(([string]$directory).Replace('\', '/'))
        }
        foreach ($directory in @(Get-ChildItem -LiteralPath $treeAbs -Directory -Recurse -Force -ErrorAction Stop)) {
            if ($cleanupState.Started.ElapsedMilliseconds -ge $DeadlineMs) { return 13 }
            $relative = $directory.FullName.Substring($treeAbs.Length).TrimStart('\').Replace('\', '/')
            if (-not $allowedDirectories.Contains($relative)) { return 12 }
        }
        foreach ($fileKey in @($proof.Keys | Sort-Object)) {
            $entry = $proof[$fileKey]
            $originalAbs = Join-Path $treeAbs ([string]$entry.OriginalPath).Replace('/', '\')
            if ($presentAtRetired.ContainsKey($fileKey)) {
                $retiredAbs = Join-Path $treeAbs ([string]$entry.RetiredPath)
                $hash = Get-DeferredFileHashWithRetry $retiredAbs
                if ($hash -ceq 'delete-pending') { continue }
                if ($hash -eq '' -or $hash -cne [string]$entry.Sha256) { return 12 }
                if ([string]$entry.RenameState -ceq 'intended') {
                    $entry.RenameState = 'renamed'
                    if (-not (Write-DeferredUninstallJournal $Journal)) { return 13 }
                }
                $retiredRemoved = $false
                try {
                    Invoke-DeferredFileDelete $retiredAbs
                    $retiredRemoved = -not (Test-Path -LiteralPath $retiredAbs)
                } catch { }
                if (-not $retiredRemoved) {
                    $retiredRecheck = Get-DeferredFileHashWithRetry $retiredAbs
                    if ($retiredRecheck -ceq 'delete-pending') {
                        Step-DeferredCrashCounter 'delete'
                        continue
                    }
                    if (-not $Journal.DeferredConsent) { return 10 }
                    $retiredRel = ([string]$TreeRecord.TreePath) + '\' + [string]$entry.RetiredPath
                    if ((Invoke-DeferredReservePath $Journal $Root $retiredRel 'file') -ne 0) { return 13 }
                    continue
                }
                Step-DeferredCrashCounter 'delete'
                continue
            }
            if (-not $presentAtOriginal.ContainsKey($fileKey)) { continue }
            if ([string]$entry.RenameState -ceq 'renamed') { return 12 }
            $hash = Get-DeferredFileHashWithRetry $originalAbs
            if ($hash -ceq 'delete-pending') { continue }
            if ($hash -eq '' -or $hash -cne [string]$entry.Sha256) { return 12 }
            $deleted = $false
            try {
                Invoke-DeferredFileDelete $originalAbs
                if (-not (Test-Path -LiteralPath $originalAbs)) { $deleted = $true }
            } catch { }
            if ($deleted) {
                Step-DeferredCrashCounter 'delete'
                continue
            }
            # The name may survive a successful POSIX disposition while a holder keeps the
            # file open; such files vanish when the holder exits and cannot be renamed.
            $recheck = Get-DeferredFileHashWithRetry $originalAbs
            if ($recheck -ceq 'delete-pending') { continue }
            if (-not $Journal.DeferredConsent) { return 10 }
            if ([string]$entry.RenameState -cne 'intended') {
                $Journal.RetireSeq = [int]$Journal.RetireSeq + 1
                $entry.RetiredPath = $DeferredRetiredPrefix + [string]$Journal.TransactionId + '-' + [int]$Journal.RetireSeq
                if (-not (Test-DeferredRetiredName ([string]$entry.RetiredPath))) { return 12 }
                $entry.RenameState = 'intended'
                if (-not (Write-DeferredUninstallJournal $Journal)) { return 13 }
                Step-DeferredCrashCounter 'retire-intent'
            }
            $retiredAbs = Join-Path $treeAbs ([string]$entry.RetiredPath)
            $retireFailure = $false
            if ($TestFixture) {
                foreach ($failurePath in @($FixtureRetireFailurePaths)) {
                    if ($failurePath.Equals($originalAbs, [StringComparison]::OrdinalIgnoreCase)) { $retireFailure = $true }
                }
            }
            if ($retireFailure) { return 13 }
            try { [IO.File]::Move($originalAbs, $retiredAbs) } catch { return 13 }
            Step-DeferredCrashCounter 'retire-rename'
            $retiredHash = Get-DeferredFileHashWithRetry $retiredAbs
            if ($retiredHash -eq '' -or $retiredHash -cne [string]$entry.Sha256) { return 12 }
            $entry.RenameState = 'renamed'
            if (-not (Write-DeferredUninstallJournal $Journal)) { return 13 }
            Step-DeferredCrashCounter 'retire'
            $retiredDeleted = $false
            try { Invoke-DeferredFileDelete $retiredAbs; $retiredDeleted = -not (Test-Path -LiteralPath $retiredAbs) } catch { }
            if (-not $retiredDeleted) {
                $retiredRel = ([string]$TreeRecord.TreePath) + '\' + [string]$entry.RetiredPath
                if ((Invoke-DeferredReservePath $Journal $Root $retiredRel 'file') -ne 0) { return 13 }
            }
        }
        $directories = @($allowedDirectories | Sort-Object @{Expression = { ($_.ToCharArray() | Where-Object { $_ -eq '/' }).Count }; Descending = $true}, @{Expression = { $_ }; Descending = $true})
        foreach ($directoryRel in $directories) {
            $directoryAbs = Join-Path $treeAbs $directoryRel.Replace('/', '\')
            if (-not (Test-Path -LiteralPath $directoryAbs -PathType Container)) { continue }
            $isEmpty = @(Get-ChildItem -LiteralPath $directoryAbs -Force -ErrorAction Stop).Count -eq 0
            $removed = $false
            if ($isEmpty) {
                try {
                    Remove-Item -LiteralPath $directoryAbs -Force -ErrorAction Stop
                    $removed = -not (Test-Path -LiteralPath $directoryAbs)
                } catch { }
            }
            if (-not $removed) {
                if (-not $Journal.DeferredConsent) { return 10 }
                $directoryRelPath = ([string]$TreeRecord.TreePath) + '\' + $directoryRel.Replace('/', '\')
                if ((Invoke-DeferredReservePath $Journal $Root $directoryRelPath 'directory') -ne 0) {
                    return 13
                }
            }
        }
        if (Test-Path -LiteralPath $treeAbs -PathType Container) {
            $treeEmpty = @(Get-ChildItem -LiteralPath $treeAbs -Force -ErrorAction Stop).Count -eq 0
            $treeRemoved = $false
            if ($treeEmpty) {
                try {
                    Remove-Item -LiteralPath $treeAbs -Force -ErrorAction Stop
                    $treeRemoved = -not (Test-Path -LiteralPath $treeAbs)
                } catch { }
            }
            if (-not $treeRemoved) {
                if (-not $Journal.DeferredConsent) { return 10 }
                if ((Invoke-DeferredReservePath $Journal $Root ([string]$TreeRecord.TreePath) 'directory') -ne 0) {
                    return 13
                }
            }
        }
        $marker = $TreeRecord.QuarantineMarker
        if ($null -ne $marker) {
            $markerAbs = Join-Path $Root ([string]$marker.Path)
            if (Test-Path -LiteralPath $markerAbs -PathType Leaf) {
                $marker.DeleteIntended = $true
                if (-not (Write-DeferredUninstallJournal $Journal)) { return 13 }
                $markerDeleted = $false
                try { Remove-Item -LiteralPath $markerAbs -Force -ErrorAction Stop; $markerDeleted = $true } catch { }
                if (-not $markerDeleted -and -not $Journal.DeferredConsent) { return 10 }
                if (-not $markerDeleted -and
                    (Invoke-DeferredReservePath $Journal $Root ([string]$marker.Path) 'file') -ne 0) { return 13 }
            }
        }
        return 0
    } catch { return 13 }
}

function Complete-DeferredBeginSide([object]$Journal) {
    if (-not (Test-DeferredJournalTrees $Journal $true)) { return 12 }
    foreach ($tree in @($Journal.Trees)) {
        $state = Test-DeferredTargetInUse $tree
        if ($state -ceq 'fail') { return 12 }
        if ($state -ceq 'inuse' -and -not [bool]$Journal.DeferredConsent) { return 10 }
    }
    $root = [IO.Path]::GetFullPath($InstallRoot).TrimEnd('\')
    $priorPath = [string]$Journal.PriorTip.Path
    try { $currentPath = Get-DeferredTipRegistrationPath } catch { return 12 }
    if ($currentPath -ne '') {
        if (-not [bool]$Journal.PriorTip.Registered -or
            -not $currentPath.Equals($priorPath, [StringComparison]::OrdinalIgnoreCase)) { return 12 }
        if (-not (Invoke-DeferredTipUnregister $priorPath)) {
            [void](Invoke-DeferredUninstallRollbackInternal $Journal)
            return 13
        }
    }
    if (-not (Test-DeferredTipAbsent)) { return 12 }
    Step-DeferredCrashCounter 'unregister'
    foreach ($tree in @($Journal.Trees)) {
        $treeAbs = Join-Path $root ([string]$tree.TreePath)
        $entry = @($tree.Files | Where-Object OriginalPath -ceq $LifetimeSentinel)[0]
        if ($null -eq $entry) {
            if ([string]$tree.Kind -ceq 'version') { return 12 }
            continue
        }
        if ([string]$entry.RenameState -ceq 'renamed') { continue }
        $entry.RetiredPath = $LifetimeSentinel + '.uninstalling'
        $entry.RenameState = 'intended'
        if (-not (Write-DeferredUninstallJournal $Journal)) { return 13 }
        $canonical = Join-Path $treeAbs $LifetimeSentinel
        if (Test-Path -LiteralPath $canonical) {
            $code = Invoke-DeferredSentinelRename $treeAbs $LifetimeSentinel ([string]$entry.RetiredPath)
            if ($code -ne 0) {
                [void](Invoke-DeferredUninstallRollbackInternal $Journal)
                return $code
            }
            Step-DeferredCrashCounter 'claim-rename'
        }
        if (-not (Test-DeferredTreeState $tree $true)) { return 12 }
        $entry.RenameState = 'renamed'
        if (-not (Write-DeferredUninstallJournal $Journal)) { return 13 }
    }
    Step-DeferredCrashCounter 'claim'
    if (Test-DeferredTasksPresent) {
        if (-not (Remove-DeferredTasks @($Journal.Tasks))) {
            [void](Invoke-DeferredUninstallRollbackInternal $Journal)
            return 13
        }
    }
    Step-DeferredCrashCounter 'tasks'
    return 0
}

function Invoke-DeferredUninstallBegin {
    $root = [IO.Path]::GetFullPath($InstallRoot).TrimEnd('\')
    $journalPath = Get-DeferredUninstallJournalPath
    if (-not [string]::IsNullOrWhiteSpace($journalPath) -and (Test-Path -LiteralPath $journalPath -PathType Leaf)) {
        $journal = Read-DeferredUninstallJournal
        if ($null -eq $journal) { return 12 }
        if ([string]$journal.Phase -cne 'pending') { return 11 }
        if ($journal.RollbackRequested) {
            if ((Invoke-DeferredUninstallRollbackInternal $journal) -ne 0) { return 12 }
            return (Invoke-DeferredUninstallBegin)
        }
        if ($DeferredConsent -and -not $journal.DeferredConsent) {
            $journal.DeferredConsent = $true
            if (-not (Write-DeferredUninstallJournal $journal)) { return 13 }
        }
        return (Complete-DeferredBeginSide $journal)
    }
    $targets = Get-DeferredUninstallTargets
    if ($null -eq $targets) { return 12 }
    foreach ($target in @($targets.Trees)) {
        $state = Test-DeferredTargetInUse $target
        if ($state -ceq 'fail') { return 12 }
        if ($state -ceq 'inuse' -and -not $DeferredConsent) { return 10 }
    }
    $priorStatus = Get-PriorTipRegistration
    if ($priorStatus -ceq 'unknown') { return 12 }
    $taskEntries = @()
    foreach ($snapshot in @(Get-DeferredTaskSnapshots)) {
        $name = [string]$snapshot.Name
        $xml = [string]$snapshot.Xml
        if (-not (Test-OwnedTaskXml $name $xml)) { return 12 }
        $taskEntries += [ordered]@{ Name = $name; Xml = $xml; XmlSha256 = (Get-TaskXmlSha256 $xml) }
    }
    $journal = [pscustomobject][ordered]@{
        Schema = 4
        Build = $UninstallBuild
        TransactionId = [Guid]::NewGuid().ToString('N')
        Phase = 'pending'
        InnoCompletion = 'unverified'
        DeferredConsent = [bool]$DeferredConsent
        RollbackRequested = $false
        PriorTip = [ordered]@{
            Registered = ($priorStatus -ceq 'registered')
            Path = (Get-DeferredTipRegistrationPath)
        }
        Tasks = @($taskEntries)
        Trees = @($targets.Trees)
        UnknownTargets = @($targets.Unknown)
        BootRounds = @()
        RetireSeq = 0
    }
    foreach ($tree in @($journal.Trees)) {
        $treeAbs = Join-Path $root ([string]$tree.TreePath)
        $tree.InitialDirectories = @(Get-ChildItem -LiteralPath $treeAbs -Directory -Recurse -Force | ForEach-Object {
            $_.FullName.Substring($treeAbs.Length).TrimStart('\').Replace('\', '/')
        })
        $tree.DeleteIntended = $false
        if ($null -ne $tree.QuarantineMarker) { $tree.QuarantineMarker.DeleteIntended = $false }
        foreach ($entry in @($tree.Files)) {
            $initial = if ([string]$entry.RenameState -ceq 'renamed') { [string]$entry.RetiredPath } else { [string]$entry.OriginalPath }
            $entry.InitialPath = if (Test-Path -LiteralPath (Join-Path $treeAbs $initial.Replace('/', '\'))) { $initial } else { '' }
            $entry.DeleteIntended = $false
        }
        if ([string]$tree.Kind -ceq 'version' -and [string]$tree.Build -ceq $UninstallBuild -and
            [string]$tree.SentinelName -ceq $LifetimeSentinel) {
            foreach ($fileEntry in @($tree.Files)) {
                if ([string]$fileEntry.OriginalPath -ceq $LifetimeSentinel) {
                    $fileEntry.RetiredPath = $LifetimeSentinel + '.uninstalling'
                    $fileEntry.RenameState = 'intended'
                }
            }
        }
    }
    if (-not (Write-DeferredUninstallJournal $journal)) { return 13 }
    Step-DeferredCrashCounter 'pending'
    return (Complete-DeferredBeginSide $journal)
}

function Invoke-DeferredUninstallAdvance {
    $root = [IO.Path]::GetFullPath($InstallRoot).TrimEnd('\')
    $journal = Read-DeferredUninstallJournal
    if ($null -eq $journal -or $journal.RollbackRequested) { return 12 }
    if ($DeferredConsent -and -not $journal.DeferredConsent) {
        $journal.DeferredConsent = $true
        if (-not (Write-DeferredUninstallJournal $journal)) { return 13 }
    }
    if ([string]$journal.Phase -ceq 'pending') {
        $code = Complete-DeferredBeginSide $journal
        if ($code -ne 0) { return $code }
        $journal.Phase = 'deleting'
        if (-not (Write-DeferredUninstallJournal $journal)) { return 13 }
        Step-DeferredCrashCounter 'deleting'
    } elseif ([string]$journal.Phase -cne 'deleting' -and
        [string]$journal.Phase -cne 'reboot-pending') { return 12 }
    if (-not (Test-DeferredTipAbsent) -or (Test-DeferredTasksPresent) -or
        -not (Test-DeferredJournalTrees $journal $false)) { return 12 }
    if ($journal.Phase -cne 'deleting') {
        $journal.Phase = 'deleting'
        $journal.InnoCompletion = 'unverified'
        if (-not (Write-DeferredUninstallJournal $journal)) { return 13 }
    }
    foreach ($tree in @($journal.Trees)) {
        $outcome = Invoke-DeferredProcessTree $root $tree $journal
        if ($outcome -ne 0) { return $outcome }
    }
    if (-not (Test-DeferredJournalTrees $journal $false $true) -or
        -not (Test-DeferredTipAbsent) -or (Test-DeferredTasksPresent)) { return 12 }
    $journal.Phase = 'reboot-pending'
    if (-not (Write-DeferredUninstallJournal $journal)) { return 13 }
    Step-DeferredCrashCounter 'rebootpending'
    return 0
}

function Invoke-DeferredUninstallComplete {
    $root = [IO.Path]::GetFullPath($InstallRoot).TrimEnd('\')
    $journalPath = Get-DeferredUninstallJournalPath
    $journal = Read-DeferredUninstallJournal
    if ($null -eq $journal) { return 12 }
    if ([string]$journal.Phase -cne 'reboot-pending') { return 12 }
    if (-not (Test-DeferredJournalTrees $journal $false $true)) { return 12 }
    $met = Test-DeferredInnoPostconditions
    if ($met -and [string]$journal.InnoCompletion -cne 'verified') {
        $journal.InnoCompletion = 'verified'
        if (-not (Write-DeferredUninstallJournal $journal)) { return 2 }
    }
    $residuals = $false
    foreach ($tree in @($journal.Trees | ForEach-Object { $_ })) {
        if (Test-Path -LiteralPath (Join-Path $root ([string]$tree.TreePath))) { $residuals = $true }
        if ($null -ne $tree.QuarantineMarker -and
            (Test-Path -LiteralPath (Join-Path $root ([string]$tree.QuarantineMarker.Path)))) { $residuals = $true }
    }
    if ($residuals) { return 5 }
    if (-not (Test-DeferredReservationBoundary $journal)) { return 5 }
    if (-not $met) { return 2 }
    try { Remove-Item -LiteralPath $journalPath -Force -ErrorAction Stop } catch { return 2 }
    return 0
}

function Invoke-DeferredUninstallRollbackInternal([object]$Journal) {
    try {
        if ([string]$Journal.Phase -cne 'pending' -or
            -not (Test-DeferredJournalTrees $Journal $true)) { return 2 }
        $root = [IO.Path]::GetFullPath($InstallRoot).TrimEnd('\')
        $currentPath = Get-DeferredTipRegistrationPath
        $priorPath = [string]$Journal.PriorTip.Path
        if ($currentPath -ne '' -and (-not [bool]$Journal.PriorTip.Registered -or
            -not $currentPath.Equals($priorPath, [StringComparison]::OrdinalIgnoreCase))) { return 2 }
        $Journal.RollbackRequested = $true
        if (-not (Write-DeferredUninstallJournal $Journal)) { return 2 }
        if (@($Journal.Tasks).Count -gt 0 -and -not (Restore-DeferredTasks @($Journal.Tasks))) { return 2 }
        foreach ($tree in @($Journal.Trees)) {
            foreach ($entry in @($tree.Files)) {
                if ([string]$entry.OriginalPath -cne $LifetimeSentinel -or
                    [string]$entry.InitialPath -cne $LifetimeSentinel -or [string]$entry.RetiredPath -eq '') { continue }
                $treeAbs = Join-Path $root ([string]$tree.TreePath)
                $entry.RenameState = 'intended'
                if (-not (Write-DeferredUninstallJournal $Journal)) { return 2 }
                if (Test-Path -LiteralPath (Join-Path $treeAbs ([string]$entry.RetiredPath))) {
                    if ((Invoke-DeferredSentinelRename $treeAbs ([string]$entry.RetiredPath) $LifetimeSentinel) -ne 0) { return 2 }
                    Step-DeferredCrashCounter 'rollback-rename'
                }
                $entry.RenameState = 'none'
                $entry.RetiredPath = ''
                if (-not (Write-DeferredUninstallJournal $Journal)) { return 2 }
            }
        }
        if (-not (Test-DeferredJournalTrees $Journal $true)) { return 2 }
        if ([bool]$Journal.PriorTip.Registered) {
            if ($currentPath -eq '' -and -not (Invoke-DeferredTipRegister $priorPath)) { return 2 }
            if (-not (Get-DeferredTipRegistrationPath).Equals($priorPath, [StringComparison]::OrdinalIgnoreCase)) { return 2 }
        } elseif (-not (Test-DeferredTipAbsent)) { return 2 }
        Remove-Item -LiteralPath (Get-DeferredUninstallJournalPath) -Force -ErrorAction Stop
        return 0
    } catch { return 2 }
}

function Remove-DeferredUninstallJournalTemps([object[]]$Temporaries, [object]$Journal = $null) {
    # The task gate excludes a live writer. Only the published journal is authoritative;
    # a replacement temp may contain torn JSON and must never be promoted to a journal.
    try {
        foreach ($temporary in $Temporaries) {
            if ($temporary.PSIsContainer -or $temporary.Length -gt $DeferredJournalMaxBytes -or
                (($temporary.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) -or
                $temporary.Name -cnotmatch '^\.nospacekey-deferred-([0-9a-f]{32})\.[0-9a-f]{32}\.tmp$') { return $false }
            if ($null -ne $Journal -and $Matches[1] -cne [string]$Journal.TransactionId) { return $false }
        }
        foreach ($temporary in $Temporaries) {
            Remove-Item -LiteralPath $temporary.FullName -Force -ErrorAction Stop
        }
        return $true
    } catch { return $false }
}

function Invoke-DeferredUninstallValidateResume {
    # 0 fresh / 20 pending / 21 deleting / 22 reboot-pending / 30 legacy / 12 blocked.
    try {
        $transactionRoot = Join-Path ([IO.Path]::GetFullPath($InstallRoot)) '.nospacekey-uninstall'
        if (-not (Test-Path -LiteralPath $transactionRoot -PathType Container)) { return 0 }
        if (-not (Test-SafeDirectoryComponent $transactionRoot)) { return 12 }
        $entries = @(Get-ChildItem -LiteralPath $transactionRoot -Force)
        if (@($entries | Where-Object { $_.PSIsContainer -or ($_.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0 }).Count -ne 0) { return 12 }
        $deferred = @($entries | Where-Object { $_.Name -cmatch '^deferred-.+\.json$' })
        if ($deferred.Count -eq 0) {
            foreach ($entry in $entries) {
                if ($entry.Name -cmatch '^(?:pending|deleting|committed)-.+\.json$' -and $entries.Count -eq 1 -and
                    $null -ne (Read-UninstallTaskJournal $entry.FullName)) { return 30 }
            }
            if ($entries.Count -ne 0) {
                # Stale deferred temps without a journal are the residue of an
                # interrupted first write; sweep them so a crashed begin can be
                # retried instead of aborting every future uninstall.
                $deferredTemps = @($entries | Where-Object {
                        -not $_.PSIsContainer -and
                        $_.Name -cmatch '^\.nospacekey-deferred-[0-9a-f]{32}\.[0-9a-f]{32}\.tmp$' })
                if ($deferredTemps.Count -ne $entries.Count) { return 12 }
                if (-not (Remove-DeferredUninstallJournalTemps $deferredTemps)) { return 12 }
                return 0
            }
            return 0
        }
        $deferredTemps = @($entries | Where-Object {
                $_.Name -cmatch '^\.nospacekey-deferred-[0-9a-f]{32}\.[0-9a-f]{32}\.tmp$' })
        if ($deferred.Count -gt 1 -or $entries.Count -ne (1 + $deferredTemps.Count)) { return 12 }
        $build = $deferred[0].BaseName.Substring('deferred-'.Length)
        if ($build -cne $UninstallBuild) { return 12 }
        $journal = Read-DeferredUninstallJournal $deferred[0].FullName
        if ($null -eq $journal -or
            -not (Remove-DeferredUninstallJournalTemps $deferredTemps $journal)) { return 12 }
        switch ([string]$journal.Phase) {
            'pending' { return 20 }
            'deleting' { return 21 }
            'reboot-pending' { return 22 }
        }
        return 12
    } catch { return 12 }
}

function Invoke-DeferredUninstallCollect([string]$JournalPath) {
    # Next-installer collection. Reboot and Inno residue require different guidance.
    try {
        $journal = Read-DeferredUninstallJournal $JournalPath
        if ($null -eq $journal) { return 'fail' }
        if ([string]$journal.Phase -ceq 'pending') {
            if ((Invoke-DeferredUninstallRollbackInternal $journal) -eq 0) { return 'ok' }
            return 'fail'
        }
        $root = [IO.Path]::GetFullPath($InstallRoot).TrimEnd('\')
        $unfinished = [string]$journal.Phase -ceq 'deleting'
        if (-not (Test-DeferredJournalTrees $journal $false (-not $unfinished))) { return 'fail' }
        if (-not $unfinished -and -not (Test-DeferredReservationBoundary $journal)) { return 'blocked' }
        $debris = $false
        foreach ($tree in @($journal.Trees | ForEach-Object { $_ })) {
            if (Test-Path -LiteralPath (Join-Path $root ([string]$tree.TreePath))) { $debris = $true }
            if ($null -ne $tree.QuarantineMarker -and
                (Test-Path -LiteralPath (Join-Path $root ([string]$tree.QuarantineMarker.Path)))) { $debris = $true }
        }
        if ($unfinished -or $debris) {
            # Advance persists deleting before starting a new reservation round and
            # supports same-boot retries without depending on a surviving uninstaller.
            $outcome = Invoke-DeferredUninstallAdvance
            if ($outcome -eq 10) { return 'resume' }
            if ($outcome -ne 0) { return 'fail' }
            $journal = Read-DeferredUninstallJournal $JournalPath
            if ($null -eq $journal) { return 'fail' }
            $stillPresent = $false
            foreach ($tree in @($journal.Trees | ForEach-Object { $_ })) {
                if (Test-Path -LiteralPath (Join-Path $root ([string]$tree.TreePath))) { $stillPresent = $true }
                if ($null -ne $tree.QuarantineMarker -and
                    (Test-Path -LiteralPath (Join-Path $root ([string]$tree.QuarantineMarker.Path)))) { $stillPresent = $true }
            }
            if ($stillPresent -or -not (Test-DeferredReservationBoundary $journal)) { return 'blocked' }
        }
        if (-not (Test-DeferredInnoPostconditions)) {
            [void](Invoke-DeferredLimitedRecovery)
            if (-not (Test-DeferredInnoPostconditions)) {
                [Console]::Error.WriteLine('Inno uninstall artifacts remain: check nospacekey shortcuts, protocol, uninstall registration, and unins000 files. Resume the original uninstaller or contact support; the recovery record is retained.')
                return 'inno-incomplete'
            }
        }
        try { Remove-Item -LiteralPath $JournalPath -Force -ErrorAction Stop } catch { return 'fail' }
        return 'ok'
    } catch { return 'fail' }
}

function Get-UninstallCallerResult([object[]]$InitialTrees) {
    $taskGate = $null
    try {
        $root = [IO.Path]::GetFullPath($InstallRoot).TrimEnd('\')
        if (-not (Test-SafeDirectoryComponent $root $true) -or
            -not (Test-DeferredInnoPostconditions $true)) { return 2 }
        $transactionRoot = Join-Path $root '.nospacekey-uninstall'
        if (Test-Path -LiteralPath $root) {
            if (-not (Test-SafeCleanupRootComponents $root $true) -or
                -not (Test-SafeDirectoryComponent $transactionRoot $true)) { return 2 }
        }
        if (Test-Path -LiteralPath $transactionRoot) {
            $entries = @(Get-ChildItem -LiteralPath $transactionRoot -Force -ErrorAction Stop)
            if ($entries.Count -ne 0) {
                $taskGate = Enter-TaskTransaction
                if ((-not $TestFixture -or $FixtureUseTaskTransactionGate) -and $null -eq $taskGate) { return 2 }
                $entries = @(Get-ChildItem -LiteralPath $transactionRoot -Force -ErrorAction Stop)
                if ($entries.Count -ne 1 -or $entries[0].FullName -cne (Get-DeferredUninstallJournalPath)) { return 2 }
                $journal = Read-DeferredUninstallJournal
                if ($null -eq $journal -or $journal.Phase -cne 'reboot-pending' -or
                    -not (Test-DeferredJournalTrees $journal $false $true)) { return 2 }
                if (-not (Test-DeferredReservationBoundary $journal)) { return 3010 }
            }
        }
        foreach ($tree in $InitialTrees) {
            if (Test-Path -LiteralPath (Join-Path $root ([string]$tree.TreePath))) { return 2 }
            if ($null -ne $tree.QuarantineMarker -and
                (Test-Path -LiteralPath (Join-Path $root ([string]$tree.QuarantineMarker.Path)))) { return 2 }
        }
        return 0
    } catch { return 2 }
    finally { if ($null -ne $taskGate) { $taskGate.Dispose() } }
}

function Invoke-UninstallAndWait {
    # Inno returns zero to its parent before its clone has completed and does not
    # export UninstallNeedRestart as an exit code. This external entry point waits
    # for the process tree, then derives 0 / 3010 / 2 from actual postconditions.
    $taskGate = $null
    $process = $null
    try {
        $root = [IO.Path]::GetFullPath($InstallRoot).TrimEnd('\')
        $programFiles = [IO.Path]::GetFullPath($env:ProgramFiles).TrimEnd('\')
        if ($TestFixture) {
            if ($root.StartsWith($programFiles + '\', [StringComparison]::OrdinalIgnoreCase)) { return 2 }
        } else {
            if (-not $root.Equals((Join-Path $programFiles 'nospacekey'), [StringComparison]::OrdinalIgnoreCase)) { return 2 }
            $principal = [Security.Principal.WindowsPrincipal]::new([Security.Principal.WindowsIdentity]::GetCurrent())
            if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { return 2 }
        }
        if (-not (Test-SafeBuild $UninstallBuild) -or
            -not (Test-SafeCleanupRootComponents $root $true)) { return 2 }
        $taskGate = Enter-TaskTransaction
        if ((-not $TestFixture -or $FixtureUseTaskTransactionGate) -and $null -eq $taskGate) { return 2 }
        $cleanupState.Started.Restart()
        $resume = Invoke-DeferredUninstallValidateResume
        if ($resume -eq 0) {
            $targets = Get-DeferredUninstallTargets
            if ($null -eq $targets) { return 2 }
            $initialTrees = @($targets.Trees)
        } elseif ($resume -in @(20, 21, 22)) {
            $journal = Read-DeferredUninstallJournal
            if ($null -eq $journal -or
                -not (Test-DeferredJournalTrees $journal ($resume -eq 20) ($resume -eq 22))) { return 2 }
            $initialTrees = @($journal.Trees)
        } else { return 2 }
        $uninstallers = @(Get-ChildItem -LiteralPath $root -Force -ErrorAction Stop |
            Where-Object { $_.Name -cmatch '^unins[0-9]{3}\.exe$' })
        if ($uninstallers.Count -ne 1 -or $uninstallers[0].PSIsContainer -or
            ($uninstallers[0].Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { return 2 }
        if (-not $TestFixture -and
            -not (Test-SemanticallyProtectedAcl (Get-Acl -LiteralPath $uninstallers[0].FullName))) { return 2 }
        # The child needs the same gate and must be able to remove the recovery
        # script. Keep only the validated paths/proof in memory across the launch.
        if ($null -ne $taskGate) { $taskGate.Dispose(); $taskGate = $null }
        $arguments = @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART')
        if ($DeferredConsent) { $arguments += '/DEFERREDDELETE' }
        $process = Start-Process -FilePath $uninstallers[0].FullName -ArgumentList $arguments `
            -WindowStyle Hidden -Wait -PassThru
        if ($process.ExitCode -ne 0) { return 2 }
        $cleanupState.Started.Restart()
        return (Get-UninstallCallerResult $initialTrees)
    } catch { return 2 }
    finally {
        if ($null -ne $taskGate) { $taskGate.Dispose() }
        if ($null -ne $process) { $process.Dispose() }
    }
}

function Get-ManifestVersion([string]$Tree) {
    try {
        $record = Get-Content -Raw -LiteralPath (Join-Path $Tree 'version-manifest.json') | ConvertFrom-Json
        return [string]$record.Version
    } catch { return '' }
}

function Read-QuarantineMarker([string]$Tree) {
    try {
        $markerPath = $Tree + '.retry.json'
        $item = Get-Item -LiteralPath $markerPath -Force
        if ($item.PSIsContainer -or $item.Length -gt 1048576 -or
            (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) { return $null }
        $record = Get-Content -Raw -LiteralPath $markerPath | ConvertFrom-Json
        if ([int]$record.Schema -ne 2 -or [string]$record.Phase -notin @('ready', 'deleting') -or
            [string]$record.Quarantine -cne (Split-Path $Tree -Leaf) -or
            [string]$record.Build -notmatch '\A[0-9A-Za-z.+-]{1,80}\z' -or
            [string]$record.ManifestSha256 -notmatch '\A[0-9A-F]{64}\z') { return $null }
        $files = @($record.Files | ForEach-Object { $_ })
        if ($files.Count -lt 5 -or $files.Count -gt 8192) { return $null }
        $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
        foreach ($entry in $files) {
            $relative = [string]$entry.Path
            if ([string]::IsNullOrWhiteSpace($relative) -or $relative.Contains('..') -or
                $relative.Contains('\') -or
                [IO.Path]::IsPathRooted($relative) -or $relative.Contains(':') -or
                $relative -in @($LifetimeSentinel, $QuarantiningSentinel) -or
                -not $seen.Add($relative) -or [string]$entry.Sha256 -notmatch '\A[0-9A-F]{64}\z') { return $null }
        }
        return $record
    } catch { return $null }
}

function Write-QuarantineMarker(
    [string]$Tree,
    [string]$Build,
    [string]$ManifestSha256,
    $Files,
    [string]$Phase = 'ready'
) {
    $markerPath = $Tree + '.retry.json'
    $temporary = $markerPath + '.' + [Guid]::NewGuid().ToString('N') + '.tmp'
    try {
        if ($Phase -notin @('ready', 'deleting')) { return $false }
        $json = [ordered]@{
            Schema = 2
            Phase = $Phase
            Build = $Build
            Quarantine = (Split-Path $Tree -Leaf)
            ManifestSha256 = $ManifestSha256
            Files = @($Files)
        } | ConvertTo-Json -Depth 4 -Compress
        $bytes = [Text.UTF8Encoding]::new($false).GetBytes($json)
        if (@($Files).Count -lt 5 -or @($Files).Count -gt 8192 -or $bytes.Length -gt 1048576) { return $false }
        [IO.File]::WriteAllBytes($temporary, $bytes)
        if (Test-Path -LiteralPath $markerPath) {
            if (-not [Nospacekey.AtomicFile]::Replace($temporary, $markerPath)) {
                throw 'atomic quarantine marker replacement failed'
            }
        } else {
            [IO.File]::Move($temporary, $markerPath)
        }
        return $true
    } catch {
        if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary -Force -ErrorAction SilentlyContinue }
        return $false
    }
}

function Test-BuildLeaseInactive([string]$Build) {
    # Returns 'inactive', 'active', or 'fail' (I/O failure). D-1 requires the
    # in-use verdict to come from an actual lease; an unreadable system state is
    # a validation failure and must not be presented as in use.
    if ($TestFixture -and -not $FixtureUseRealLeases) {
        if ($FixtureLeaseProbeFailures -contains $Build) { return 'fail' }
        if ($FixtureActiveBuilds -notcontains $Build) { return 'inactive' }
        return 'active'
    }
    try {
        $sessions = @(Get-Process -ErrorAction Stop | Select-Object -ExpandProperty SessionId -Unique)
        if ($sessions.Count -eq 0) { return 'fail' }
        foreach ($session in $sessions) {
            $created = $false
            $lease = [Threading.Mutex]::new($false, "Global\nospacekey-version-lease-$Build-s$session", [ref]$created)
            $lease.Dispose()
            if (-not $created) { return 'active' }
        }
        return 'inactive'
    } catch { return 'fail' }
}

function Get-ProtectedTargets {
    if ($TestFixture) {
        $taskTargets = @($FixtureTaskTargets)
        if (-not [string]::IsNullOrWhiteSpace($FixtureTaskTargetsPath)) {
            $parsedTargets = Get-Content -Raw -LiteralPath $FixtureTaskTargetsPath | ConvertFrom-Json
            $taskTargets = if ($null -eq $parsedTargets) {
                @()
            } else {
                @($parsedTargets | ForEach-Object { [string]$_ })
            }
        }
        return @($FixtureCurrentTipPath) + $taskTargets
    }
    $key = [Microsoft.Win32.RegistryKey]::OpenBaseKey(
        [Microsoft.Win32.RegistryHive]::LocalMachine,
        [Microsoft.Win32.RegistryView]::Registry64).OpenSubKey(
        'Software\Classes\CLSID\{B4B39227-EFF2-41DA-B357-0C3170A57875}\InprocServer32', $false)
    if ($null -eq $key) { throw 'current COM target unavailable' }
    try { $tip = [string]$key.GetValue('', $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames) }
    finally { $key.Dispose() }
    if ([string]::IsNullOrWhiteSpace($tip)) { throw 'current COM target empty' }
    $tasks = @(Get-ScheduledTask -ErrorAction Stop | Where-Object {
        $_.TaskPath -eq '\nospacekey\' -and $_.TaskName -like 'UpdateCheck-*'
    })
    $targets = @($tip)
    foreach ($task in $tasks) {
        foreach ($action in @($task.Actions)) {
            if ([string]::IsNullOrWhiteSpace([string]$action.Execute)) { throw 'scheduled task target unavailable' }
            $targets += [Environment]::ExpandEnvironmentVariables([string]$action.Execute).Trim('"')
        }
    }
    return $targets
}

function Test-TreeProtected([string]$Tree, [string[]]$Targets) {
    $prefix = [IO.Path]::GetFullPath($Tree).TrimEnd('\') + '\'
    foreach ($target in $Targets) {
        try { $full = [IO.Path]::GetFullPath($target) } catch { return $true }
        if ($full.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) { return $true }
    }
    return $false
}

function Get-ValidatedProtectedTargets([string]$Versions) {
    $targets = @(Get-ProtectedTargets)
    $versionsPrefix = [IO.Path]::GetFullPath($Versions).TrimEnd('\') + '\'
    foreach ($target in $targets) {
        try { $targetFull = [IO.Path]::GetFullPath($target) } catch { throw 'protected target path is invalid' }
        if (-not $targetFull.StartsWith($versionsPrefix, [StringComparison]::OrdinalIgnoreCase)) {
            throw 'protected target is outside the owned versions root'
        }
    }
    return $targets
}

function Invoke-VersionCleanup {
    Write-FixtureCleanupTrace 'cleanup-start' 'validating cleanup bounds and root'
    $root = [IO.Path]::GetFullPath($InstallRoot).TrimEnd('\')
    if ($MaxTrees -lt 1 -or $MaxTrees -gt 8 -or $DeadlineMs -lt 100 -or $DeadlineMs -gt 15000 -or
        $CoordinationWaitMs -lt 100 -or $CoordinationWaitMs -gt 300000) { throw 'invalid bounds' }
    if ($TestFixture) {
        $programFiles = [IO.Path]::GetFullPath($env:ProgramFiles).TrimEnd('\') + '\'
        if ($root.StartsWith($programFiles, [StringComparison]::OrdinalIgnoreCase)) { throw 'fixture cannot target Program Files' }
    } else {
        $expected = [IO.Path]::GetFullPath((Join-Path $env:ProgramFiles 'nospacekey')).TrimEnd('\')
        if (-not $root.Equals($expected, [StringComparison]::OrdinalIgnoreCase)) { throw 'unexpected install root' }
        $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
        if (-not ([Security.Principal.WindowsPrincipal]$identity).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { throw 'cleanup requires elevation' }
    }
    $versions = Join-Path $root 'versions'
    if (-not (Test-SafeCleanupRootComponents $root)) { throw 'cleanup root is unsafe' }
    Write-FixtureCleanupTrace 'protected-targets' 'reading protected COM and scheduled-task targets'
    $targets = @(Get-ValidatedProtectedTargets $versions)
    $inspectLimit = $MaxTrees * 8

    # The worker holds the task gate before scanning, including when no journal exists
    # yet. Existing records also protect their trees between uninstall invocations.
    $deferredSkipPaths = @()
    $deferredJournalRoot = Join-Path $root '.nospacekey-uninstall'
    if (Test-Path -LiteralPath $deferredJournalRoot -PathType Container) {
        $deferredJournals = @(Get-ChildItem -LiteralPath $deferredJournalRoot -File -Force |
            Where-Object { $_.Name -cmatch '^deferred-.+\.json$' })
        if ($deferredJournals.Count -gt 0) {
            foreach ($deferredJournal in $deferredJournals) {
                $deferredBuild = $deferredJournal.BaseName.Substring('deferred-'.Length)
                if (-not (Test-SafeBuild $deferredBuild)) { throw 'invalid deferred uninstall journal name' }
                $script:UninstallBuild = $deferredBuild
                $deferredRecord = Read-DeferredUninstallJournal $deferredJournal.FullName
                if ($null -eq $deferredRecord) { throw 'unreadable deferred uninstall journal' }
                foreach ($deferredTree in @($deferredRecord.Trees | ForEach-Object { $_ })) {
                    $deferredSkipPaths += [string]$deferredTree.TreePath
                    if ($null -ne $deferredTree.QuarantineMarker) {
                        $deferredSkipPaths += [string]$deferredTree.QuarantineMarker.Path
                    }
                }
            }
        }
    }

    # Old quarantines are retried first. Newly renamed trees are deliberately not deleted
    # in this invocation, leaving a full cleanup-intent cycle for racing loaders to unload.
    Write-FixtureCleanupTrace 'retry-scan' 'validating existing quarantine retries'
    foreach ($tree in @(Get-ChildItem -LiteralPath $versions -Directory -Force |
            Where-Object Name -like '.cleanup-*' | Select-Object -First $inspectLimit)) {
        if ($cleanupState.Processed -ge $MaxTrees -or $cleanupState.Started.ElapsedMilliseconds -ge $DeadlineMs) { break }
        if ($deferredSkipPaths -contains ('versions\' + $tree.Name)) { continue }
        if (-not (Test-SafeDirectoryComponent $tree.FullName) -or
            -not (Test-ReparseFreeTree $tree.FullName)) { continue }
        $canonical = Join-Path $tree.FullName $LifetimeSentinel
        $transition = Join-Path $tree.FullName $QuarantiningSentinel
        $hasCanonical = Test-Path -LiteralPath $canonical -PathType Leaf
        $hasTransition = Test-Path -LiteralPath $transition -PathType Leaf
        if ($hasCanonical -and $hasTransition) { continue }
        if (-not $hasCanonical -and -not $hasTransition) {
            $inertMarker = Read-QuarantineMarker $tree.FullName
            if ($null -eq $inertMarker -or [string]$inertMarker.Phase -cne 'deleting' -or
                -not (Test-QuarantineRemainingSubset $tree.FullName $inertMarker)) { continue }
            Restore-QuarantiningSentinelAfterDeleteFailure $tree.FullName
            $hasTransition = Test-Path -LiteralPath $transition -PathType Leaf
            if (-not $hasTransition) { continue }
        }
        if (-not (Wait-FixtureCleanupBarrier $FixtureBeforeRetryClaimReadyPath $FixtureBeforeRetryClaimContinuePath $tree.Name)) {
            Exit-FixtureCleanupFailure 'before-retry-claim barrier deadline'
        }
        $claim = Open-LifetimeSentinelPathClaim $(if ($hasTransition) { $transition } else { $canonical })
        if ($null -eq $claim) { continue }
        $retryTransitioned = $hasTransition
        try {
            if (-not (Test-SafeDirectoryComponent $tree.FullName) -or
                -not (Test-ReparseFreeTree $tree.FullName)) { continue }
            $marker = Read-QuarantineMarker $tree.FullName
            if ($null -eq $marker) { continue }
            $build = [string]$marker.Build
            $finalTargets = @(Get-ValidatedProtectedTargets $versions)
            if (Test-TreeProtected $tree.FullName $finalTargets) { continue }
            if ((Test-BuildLeaseInactive $build) -cne 'inactive') { continue }
            if ([string]$marker.Phase -ceq 'deleting') {
                if (-not $retryTransitioned) { continue }
                if (-not (Test-QuarantineRemainingSubset $tree.FullName $marker)) { continue }
            } else {
                $manifestPath = Join-Path $tree.FullName 'version-manifest.json'
                if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf) -or
                    (Get-FileHash -LiteralPath $manifestPath -Algorithm SHA256).Hash -cne
                        [string]$marker.ManifestSha256) { continue }
                $manifestSentinelAlias = if ($retryTransitioned) { $QuarantiningSentinel } else { '' }
                $proofSentinelAlias = if ($retryTransitioned) { $QuarantiningSentinel } else { $LifetimeSentinel }
                try { if ($null -eq (Read-OwnedManifest $tree.FullName $build $manifestSentinelAlias $true)) { continue } }
                catch { continue }
                if (-not (Test-QuarantineRemainingSubset $tree.FullName $marker $true $proofSentinelAlias)) { continue }
                if (-not $retryTransitioned) {
                    if (-not $claim.Rename($transition)) { continue }
                    $retryTransitioned = $true
                }
                if (-not (Write-QuarantineMarker -Tree $tree.FullName -Build $build `
                        -ManifestSha256 ([string]$marker.ManifestSha256) `
                        -Files @($marker.Files | ForEach-Object { $_ }) -Phase 'deleting')) { continue }
                $marker = Read-QuarantineMarker $tree.FullName
                if ($null -eq $marker -or [string]$marker.Phase -cne 'deleting') { continue }
            }
            if (-not (Wait-FixtureCleanupBarrier $FixtureAfterRetryClaimReadyPath $FixtureAfterRetryClaimContinuePath $build)) {
                Exit-FixtureCleanupFailure 'after-retry-claim barrier deadline'
            }
            Remove-QuarantineOwnedContent $tree.FullName $build
            $claim.Dispose()
            $claim = $null
            Remove-Item -LiteralPath $transition -Force -ErrorAction Stop
            if ($TestFixture -and $FixtureCrashAfterRetryTransitionRemovedBuilds -contains $build) { exit 86 }
            Remove-Item -LiteralPath $tree.FullName -Force -ErrorAction Stop
            Remove-Item -LiteralPath ($tree.FullName + '.retry.json') -Force -ErrorAction SilentlyContinue
            $cleanupState.Removed++
            $cleanupState.Processed++
        } catch {
            if ($retryTransitioned) { Restore-QuarantiningSentinelAfterDeleteFailure $tree.FullName }
        }
        finally { if ($null -ne $claim) { $claim.Dispose() } }
    }

    Write-FixtureCleanupTrace 'tree-scan' 'validating version trees for quarantine'
    foreach ($tree in @(Get-ChildItem -LiteralPath $versions -Directory -Force |
            Where-Object Name -notlike '.cleanup-*' | Sort-Object Name |
            Select-Object -First $inspectLimit)) {
        if ($cleanupState.Processed -ge $MaxTrees -or $cleanupState.Started.ElapsedMilliseconds -ge $DeadlineMs) { break }
        if ($deferredSkipPaths -contains ('versions\' + $tree.Name)) { continue }
        $build = $tree.Name
        if (Test-TreeProtected $tree.FullName $targets) { continue }
        if (-not (Test-SafeVersionPathComponents $root $build) -or
            (Test-BuildLeaseInactive $build) -cne 'inactive' -or -not (Test-ReparseFreeTree $tree.FullName)) { continue }
        if (-not (Restore-LifetimeSentinel $tree.FullName)) { continue }
        try { if ($null -eq (Read-OwnedManifest $tree.FullName $build)) { continue } } catch { continue }
        $manifestHash = (Get-FileHash -LiteralPath (Join-Path $tree.FullName 'version-manifest.json') -Algorithm SHA256).Hash
        $deleteProof = New-QuarantineDeleteProof $tree.FullName
        if ($null -eq $deleteProof) { continue }
        $quarantine = $null
        foreach ($markerFile in @(Get-ChildItem -LiteralPath $versions -File -Filter '.cleanup-*.retry.json' | Select-Object -First $inspectLimit)) {
            $precommitted = $markerFile.FullName.Substring(0, $markerFile.FullName.Length - '.retry.json'.Length)
            $precommitMarker = Read-QuarantineMarker $precommitted
            if ($null -ne $precommitMarker -and [string]$precommitMarker.Phase -ceq 'ready' -and
                [string]$precommitMarker.Build -ceq $build -and
                [string]$precommitMarker.ManifestSha256 -ceq $manifestHash -and -not (Test-Path -LiteralPath $precommitted)) {
                $quarantine = $precommitted
                break
            }
        }
        if ($null -eq $quarantine) {
            $quarantine = Join-Path $versions ('.cleanup-{0}' -f [Guid]::NewGuid().ToString('N'))
            if (-not (Write-QuarantineMarker $quarantine $build $manifestHash $deleteProof)) { continue }
        }
        if ($TestFixture -and $FixtureCrashAfterMarkerBuilds -contains $build) { exit 86 }
        if ($TestFixture -and -not [string]::IsNullOrWhiteSpace($FixtureBeforeRenameReadyPath)) {
            [IO.File]::WriteAllText($FixtureBeforeRenameReadyPath, $build)
            while (-not (Test-Path -LiteralPath $FixtureBeforeRenameContinuePath)) {
                if ($cleanupState.Started.ElapsedMilliseconds -ge $DeadlineMs) {
                    Exit-FixtureCleanupFailure 'before-rename barrier deadline'
                }
                Start-Sleep -Milliseconds 10
            }
        }
        $claim = Open-LifetimeSentinelClaim $tree.FullName
        if ($null -eq $claim) { continue }
        try {
            if (-not (Test-SafeVersionPathComponents $root $build)) { continue }
            $finalTargets = @(Get-ValidatedProtectedTargets $versions)
            if (Test-TreeProtected $tree.FullName $finalTargets) {
                continue
            }
            $quarantiningSentinel = Join-Path $tree.FullName $QuarantiningSentinel
            if (-not $claim.Rename($quarantiningSentinel)) {
                continue
            }
            $claim.Dispose()
            $claim = $null
            if ($TestFixture -and $FixtureCrashAfterClaimBuilds -contains $build) { exit 86 }
            try {
                [IO.Directory]::Move($tree.FullName, $quarantine)
            } catch { continue }
            [void][Nospacekey.AtomicFile]::Replace(
                (Join-Path $quarantine $QuarantiningSentinel),
                (Join-Path $quarantine $LifetimeSentinel))
        } catch { continue }
        finally { if ($null -ne $claim) { $claim.Dispose() } }
        $cleanupState.Processed++
    }
    Write-FixtureCleanupTrace 'worker-complete' "removed=$($cleanupState.Removed) processed=$($cleanupState.Processed)"
    return $cleanupState.Removed
}

if ($FixtureWaitOwnedMutexOnly) {
    $fixtureMutex = $null
    $fixtureMutexOwned = $false
    try {
        if (-not $TestFixture -or [string]::IsNullOrWhiteSpace($FixtureGateName)) { exit 2 }
        $fixtureMutex = [Threading.Mutex]::new($false, $FixtureGateName)
        $fixtureMutexOwned = Wait-OwnedMutex $fixtureMutex $CoordinationWaitMs
        if ($fixtureMutexOwned) { exit 0 }
    } catch { }
    finally {
        if ($fixtureMutexOwned) { try { $fixtureMutex.ReleaseMutex() } catch { } }
        if ($null -ne $fixtureMutex) { $fixtureMutex.Dispose() }
    }
    exit 2
}

if ($FixtureHoldTaskGateMs -gt 0) {
    $taskGate = $null
    try {
        if (-not $TestFixture -or -not $FixtureUseTaskTransactionGate -or
            [string]::IsNullOrWhiteSpace($FixtureTaskGateReadyPath)) { exit 2 }
        $taskGate = Enter-TaskTransaction
        if ($null -eq $taskGate) { exit 2 }
        [IO.File]::WriteAllText($FixtureTaskGateReadyPath, 'held')
        Start-Sleep -Milliseconds $FixtureHoldTaskGateMs
        exit 0
    } catch { exit 2 }
    finally { if ($null -ne $taskGate) { $taskGate.Dispose() } }
}

if ($FixtureValidateAclSddl) {
    try {
        if (-not $TestFixture -or [string]::IsNullOrWhiteSpace($FixtureAclSddl)) { exit 2 }
        $acl = [Security.AccessControl.DirectorySecurity]::new()
        $acl.SetSecurityDescriptorSddlForm($FixtureAclSddl)
        if (Test-SemanticallyProtectedAcl $acl) { exit 0 }
    } catch { }
    exit 2
}

if ($FixtureValidateTaskArtifactAclSddl) {
    try {
        if (-not $TestFixture -or
            [string]::IsNullOrWhiteSpace($FixtureTaskGateAclSddl) -or
            [string]::IsNullOrWhiteSpace($FixtureTaskJournalAclSddl)) { exit 2 }
        $gateAcl = [Security.AccessControl.RawSecurityDescriptor]::new($FixtureTaskGateAclSddl)
        $journalAcl = [Security.AccessControl.RawSecurityDescriptor]::new($FixtureTaskJournalAclSddl)
        if (Test-TaskTransactionArtifactAclObjects $gateAcl $journalAcl) { exit 0 }
    } catch { }
    exit 2
}

if (-not [string]::IsNullOrWhiteSpace($FixtureValidateOwnedBuild)) {
    try {
        if (-not $TestFixture) { exit 2 }
        $cleanupState.Started.Restart()
        $DeadlineMs = [int]::MaxValue
        $tree = Get-VersionTree $InstallRoot $FixtureValidateOwnedBuild
        if ($null -ne $tree -and (Test-ReparseFreeTree $tree) -and
            $null -ne (Read-OwnedManifest $tree $FixtureValidateOwnedBuild)) { exit 0 }
    } catch { }
    exit 2
}

if ($RecoverUninstallClaim) {
    try {
        $cleanupState.Started.Restart()
        if (Recover-VersionTreeUninstallClaim $InstallRoot $UninstallBuild) { exit 0 }
    } catch { }
    exit 2
}

if ($ClaimForUninstall) {
    try {
        $cleanupState.Started.Restart()
        if (Claim-VersionTreeForUninstall $InstallRoot $UninstallBuild) { exit 0 }
    } catch { }
    exit 2
}

if ($CommitUninstallTasks) {
    $taskGate = $null
    try {
        $taskGate = Enter-TaskTransaction
        if ((-not $TestFixture -or $FixtureUseTaskTransactionGate) -and $null -eq $taskGate) { exit 2 }
        if (Commit-UninstallTasks) { exit 0 }
    } catch { }
    finally { if ($null -ne $taskGate) { $taskGate.Dispose() } }
    exit 2
}

if ($RecoverUninstallTasks) {
    $taskGate = $null
    try {
        $taskGate = Enter-TaskTransaction
        if ((-not $TestFixture -or $FixtureUseTaskTransactionGate) -and $null -eq $taskGate) { exit 2 }
        if (Recover-PendingUninstall) { exit 0 }
    } catch { }
    finally { if ($null -ne $taskGate) { $taskGate.Dispose() } }
    exit 2
}

if ($FinalizeUninstallTasks) {
    $taskGate = $null
    try {
        $taskGate = Enter-TaskTransaction
        if ((-not $TestFixture -or $FixtureUseTaskTransactionGate) -and $null -eq $taskGate) { exit 2 }
        if (Finalize-UninstallTasks) { exit 0 }
    } catch { }
    finally { if ($null -ne $taskGate) { $taskGate.Dispose() } }
    exit 2
}

if ($ValidateDeletingUninstall) {
    $taskGate = $null
    try {
        $taskGate = Enter-TaskTransaction
        if ((-not $TestFixture -or $FixtureUseTaskTransactionGate) -and $null -eq $taskGate) { exit 2 }
        if (Test-DeletingUninstall $UninstallBuild) { exit 0 }
        $deletingRoot = Join-Path ([IO.Path]::GetFullPath($InstallRoot)) '.nospacekey-uninstall'
        if (-not (Test-Path -LiteralPath $deletingRoot) -or
            ((Test-Path -LiteralPath $deletingRoot -PathType Container) -and
            @(Get-ChildItem -LiteralPath $deletingRoot -Force).Count -eq 0)) {
            exit 3
        }
    } catch { }
    finally { if ($null -ne $taskGate) { $taskGate.Dispose() } }
    exit 2
}

if ($RecoverInterruptedUninstalls) {
    $taskGate = $null
    try {
        $taskGate = Enter-TaskTransaction
        if ((-not $TestFixture -or $FixtureUseTaskTransactionGate) -and $null -eq $taskGate) { exit 2 }
        if (Recover-InterruptedUninstalls) {
            if (-not [string]::IsNullOrWhiteSpace($script:InterruptedDeletingBuild)) { exit 3 }
            if ($script:DeferredCollectBlocked) { exit 4 }
            if ($script:DeferredInnoIncomplete) { exit 5 }
            exit 0
        }
    } catch { }
    finally { if ($null -ne $taskGate) { $taskGate.Dispose() } }
    exit 2
}

if ($InitializeTaskTransactionArtifacts) {
    try {
        if (Initialize-TaskTransactionArtifacts) { exit 0 }
    } catch { }
    exit 2
}

if ($ValidateFinalUninstallArtifacts) {
    try {
        if (Test-FinalUninstallArtifacts) { exit 0 }
    } catch { }
    exit 2
}

if ($RunUninstall) {
    exit (Invoke-UninstallAndWait)
}

if ($ProbeUninstallTargets) {
    try {
        $cleanupState.Started.Restart()
        $targets = Get-DeferredUninstallTargets
        if ($null -eq $targets) { exit 12 }
        foreach ($target in @($targets.Trees)) {
            $state = Test-DeferredTargetInUse $target
            if ($state -ceq 'fail') { exit 12 }
            if ($state -ceq 'inuse') { exit 10 }
        }
        exit 0
    } catch { exit 12 }
}

if ($ValidateDeferredUninstallResume) {
    $taskGate = $null
    try {
        $taskGate = Enter-TaskTransaction
        if ((-not $TestFixture -or $FixtureUseTaskTransactionGate) -and $null -eq $taskGate) { exit 12 }
        exit (Invoke-DeferredUninstallValidateResume)
    } catch { exit 12 }
    finally { if ($null -ne $taskGate) { $taskGate.Dispose() } }
}

if ($QueryDeferredUninstallRestart) {
    $taskGate = $null
    try {
        $taskGate = Enter-TaskTransaction
        if ((-not $TestFixture -or $FixtureUseTaskTransactionGate) -and $null -eq $taskGate) { exit 13 }
        $journal = Read-DeferredUninstallJournal
        if ($null -eq $journal -or $journal.Phase -cne 'reboot-pending' -or
            -not (Test-DeferredJournalTrees $journal $false $true)) { exit 12 }
        if (-not (Test-DeferredReservationBoundary $journal)) { exit 5 }
        exit 0
    } catch { exit 12 }
    finally { if ($null -ne $taskGate) { $taskGate.Dispose() } }
}

if ($BeginDeferredUninstall) {
    $taskGate = $null
    try {
        $taskGate = Enter-TaskTransaction
        if ((-not $TestFixture -or $FixtureUseTaskTransactionGate) -and $null -eq $taskGate) { exit 13 }
        exit (Invoke-DeferredUninstallBegin)
    } catch { exit 13 }
    finally { if ($null -ne $taskGate) { $taskGate.Dispose() } }
}

if ($AdvanceDeferredUninstall) {
    $taskGate = $null
    try {
        $taskGate = Enter-TaskTransaction
        if ((-not $TestFixture -or $FixtureUseTaskTransactionGate) -and $null -eq $taskGate) { exit 13 }
        exit (Invoke-DeferredUninstallAdvance)
    } catch { exit 13 }
    finally { if ($null -ne $taskGate) { $taskGate.Dispose() } }
}

if ($CompleteDeferredUninstall) {
    $taskGate = $null
    try {
        $taskGate = Enter-TaskTransaction
        if ((-not $TestFixture -or $FixtureUseTaskTransactionGate) -and $null -eq $taskGate) { exit 13 }
        exit (Invoke-DeferredUninstallComplete)
    } catch { exit 13 }
    finally { if ($null -ne $taskGate) { $taskGate.Dispose() } }
}

if ($RollbackDeferredUninstall) {
    $taskGate = $null
    try {
        $taskGate = Enter-TaskTransaction
        if ((-not $TestFixture -or $FixtureUseTaskTransactionGate) -and $null -eq $taskGate) { exit 2 }
        $journal = Read-DeferredUninstallJournal
        if ($null -eq $journal) { exit 2 }
        if ((Invoke-DeferredUninstallRollbackInternal $journal) -eq 0) { exit 0 }
        exit 2
    } catch { exit 2 }
    finally { if ($null -ne $taskGate) { $taskGate.Dispose() } }
}

if ($CleanupWorker) {
    $taskGate = $null
    try {
        $json = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($WorkerConfig))
        $config = $json | ConvertFrom-Json
        $InstallRoot = [string]$config.InstallRoot
        $MaxTrees = [int]$config.MaxTrees
        $DeadlineMs = [int]$config.DeadlineMs
        $CoordinationWaitMs = [int]$config.CoordinationWaitMs
        $TestFixture = [bool]$config.TestFixture
        $FixtureCurrentTipPath = [string]$config.FixtureCurrentTipPath
        $FixtureTaskTargets = @($config.FixtureTaskTargets | ForEach-Object { [string]$_ })
        $FixtureActiveBuilds = @($config.FixtureActiveBuilds | ForEach-Object { [string]$_ })
        $FixtureUseRealLeases = [bool]$config.FixtureUseRealLeases
        $FixtureUseTaskTransactionGate = [bool]$config.FixtureUseTaskTransactionGate
        $FixtureValidatePathAcls = [bool]$config.FixtureValidatePathAcls
        $script:FixtureTracePath = [string]$config.FixtureTracePath
        $FixtureDeleteFailureBuilds = @($config.FixtureDeleteFailureBuilds | ForEach-Object { [string]$_ })
        $FixtureCrashAfterMarkerBuilds = @($config.FixtureCrashAfterMarkerBuilds | ForEach-Object { [string]$_ })
        $FixtureCrashAfterClaimBuilds = @($config.FixtureCrashAfterClaimBuilds | ForEach-Object { [string]$_ })
        $FixtureCrashAfterRetryTransitionRemovedBuilds = @($config.FixtureCrashAfterRetryTransitionRemovedBuilds | ForEach-Object { [string]$_ })
        $FixtureTaskTargetsPath = [string]$config.FixtureTaskTargetsPath
        $FixtureBeforeRenameReadyPath = [string]$config.FixtureBeforeRenameReadyPath
        $FixtureBeforeRenameContinuePath = [string]$config.FixtureBeforeRenameContinuePath
        $FixtureBeforeRetryClaimReadyPath = [string]$config.FixtureBeforeRetryClaimReadyPath
        $FixtureBeforeRetryClaimContinuePath = [string]$config.FixtureBeforeRetryClaimContinuePath
        $FixtureAfterRetryClaimReadyPath = [string]$config.FixtureAfterRetryClaimReadyPath
        $FixtureAfterRetryClaimContinuePath = [string]$config.FixtureAfterRetryClaimContinuePath
        $FixtureOwnedModelSha256 = [string]$config.FixtureOwnedModelSha256
        $ownedModelSha256 = if ([string]::IsNullOrWhiteSpace($FixtureOwnedModelSha256)) { $InstallerModelSha256 } else { $FixtureOwnedModelSha256 }
        # The supervisor owns the registration mutex first; never wait for it while
        # holding this gate. Uninstall takes only this gate, so acquisition cannot cycle.
        $taskGate = Enter-TaskTransaction
        if ((-not $TestFixture -or $FixtureUseTaskTransactionGate) -and $null -eq $taskGate) {
            throw 'task transaction gate unavailable for cleanup'
        }
        $cleanupState.Started.Restart()
        [IO.File]::WriteAllText([string]$config.SupervisorReadyPath, [string]$PID)
        if (-not [string]::IsNullOrWhiteSpace([string]$config.FixtureWorkerPidPath)) {
            [IO.File]::WriteAllText([string]$config.FixtureWorkerPidPath, [string]$PID)
        }
        if ([int]$config.FixtureHangMs -gt 0) { Start-Sleep -Milliseconds ([int]$config.FixtureHangMs) }
        Write-FixtureCleanupTrace 'worker-invoke' 'starting version cleanup'
        [void](Invoke-VersionCleanup)
        exit 0
    } catch {
        $traceDetail = $_.Exception.GetType().FullName + ': ' + $_.Exception.Message
        if (-not [string]::IsNullOrWhiteSpace([string]$_.ScriptStackTrace)) {
            $traceDetail += ' | ' + ([string]$_.ScriptStackTrace -replace '[\r\n]+', ' ')
        }
        Write-FixtureCleanupTrace 'worker-exception' $traceDetail
        Write-Error -ErrorAction Continue $_
        exit 2
    } finally { if ($null -ne $taskGate) { $taskGate.Dispose() } }
}

$gate = $null
$child = $null
$supervisorReadyPath = $null
try {
    if ($CoordinationWaitMs -lt 100 -or $CoordinationWaitMs -gt 300000) { throw 'invalid coordination bound' }
    if (-not $TestFixture -or -not [string]::IsNullOrWhiteSpace($FixtureGateName)) {
        $gateName = if ($TestFixture) { $FixtureGateName } else { 'Global\nospacekey-tip-registration' }
        $gate = [Threading.Mutex]::new($false, $gateName)
        if (-not (Wait-OwnedMutex $gate $CoordinationWaitMs)) { exit 2 }
    }
    $supervisorReadyPath = Join-Path ([IO.Path]::GetTempPath()) ('nospacekey-cleanup-ready-' + [Guid]::NewGuid().ToString('N'))
    $config = [ordered]@{
        InstallRoot = $InstallRoot; MaxTrees = $MaxTrees; DeadlineMs = $DeadlineMs; CoordinationWaitMs = $CoordinationWaitMs
        TestFixture = [bool]$TestFixture; FixtureCurrentTipPath = $FixtureCurrentTipPath
        FixtureTaskTargets = @($FixtureTaskTargets); FixtureActiveBuilds = @($FixtureActiveBuilds)
        FixtureUseRealLeases = [bool]$FixtureUseRealLeases; FixtureHangMs = $FixtureHangMs
        FixtureUseTaskTransactionGate = [bool]$FixtureUseTaskTransactionGate
        FixtureValidatePathAcls = [bool]$FixtureValidatePathAcls
        FixtureTracePath = $FixtureTracePath
        FixtureWorkerPidPath = $FixtureWorkerPidPath; FixtureDeleteFailureBuilds = @($FixtureDeleteFailureBuilds)
        FixtureCrashAfterMarkerBuilds = @($FixtureCrashAfterMarkerBuilds)
        FixtureCrashAfterClaimBuilds = @($FixtureCrashAfterClaimBuilds)
        FixtureCrashAfterRetryTransitionRemovedBuilds = @($FixtureCrashAfterRetryTransitionRemovedBuilds)
        FixtureTaskTargetsPath = $FixtureTaskTargetsPath
        FixtureBeforeRenameReadyPath = $FixtureBeforeRenameReadyPath; FixtureBeforeRenameContinuePath = $FixtureBeforeRenameContinuePath
        FixtureBeforeRetryClaimReadyPath = $FixtureBeforeRetryClaimReadyPath
        FixtureBeforeRetryClaimContinuePath = $FixtureBeforeRetryClaimContinuePath
        FixtureAfterRetryClaimReadyPath = $FixtureAfterRetryClaimReadyPath
        FixtureAfterRetryClaimContinuePath = $FixtureAfterRetryClaimContinuePath
        FixtureOwnedModelSha256 = $FixtureOwnedModelSha256
        SupervisorReadyPath = $supervisorReadyPath
    }
    $encoded = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes(($config | ConvertTo-Json -Compress -Depth 4)))
    $start = [Diagnostics.ProcessStartInfo]::new()
    $start.FileName = (Get-Process -Id $PID).Path
    $start.Arguments = '-NoProfile -NonInteractive -ExecutionPolicy Bypass -File "' + $PSCommandPath.Replace('"','""') + '" -InstallRoot x -CleanupWorker -WorkerConfig ' + $encoded
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $child = [Diagnostics.Process]::Start($start)
    $readyWait = [Diagnostics.Stopwatch]::StartNew()
    while (-not (Test-Path -LiteralPath $supervisorReadyPath)) {
        if ($child.WaitForExit(50)) { exit $child.ExitCode }
        if ($readyWait.ElapsedMilliseconds -ge $CoordinationWaitMs) {
            try { $child.Kill() } catch { }
            if (-not $child.WaitForExit(5000) -or -not $child.HasExited) { $child.WaitForExit() }
            exit 2
        }
    }
    if (-not $child.WaitForExit($DeadlineMs)) {
        try { $child.Kill() } catch { }
        if (-not $child.WaitForExit(5000) -or -not $child.HasExited) {
            $child.WaitForExit()
        }
        exit 2
    }
    exit $child.ExitCode
} catch {
    Write-Error -ErrorAction Continue $_
    exit 2
} finally {
    if ($null -ne $supervisorReadyPath) { Remove-Item -LiteralPath $supervisorReadyPath -Force -ErrorAction SilentlyContinue }
    if ($null -ne $child) { $child.Dispose() }
    if ($null -ne $gate) { try { $gate.ReleaseMutex() } catch {}; $gate.Dispose() }
}
