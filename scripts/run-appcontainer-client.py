"""Run the IPC identity integration client in a disposable AppContainer or LPAC.

Invoked by the ignored Rust integration test; requires Python 3 on Windows.
Only the staged test directory receives read/execute ACLs. No installed IME,
existing process, or persistent AppContainer profile is modified.
"""
import argparse
import ctypes as c
from ctypes import wintypes as w
import msvcrt
import os
from pathlib import Path
import subprocess
import uuid


class StartupInfo(c.Structure):
    _fields_ = [
        ("cb", w.DWORD), ("reserved", w.LPWSTR), ("desktop", w.LPWSTR),
        ("title", w.LPWSTR), ("x", w.DWORD), ("y", w.DWORD),
        ("width", w.DWORD), ("height", w.DWORD), ("chars_x", w.DWORD),
        ("chars_y", w.DWORD), ("fill", w.DWORD), ("flags", w.DWORD),
        ("show", w.WORD), ("reserved_size", w.WORD), ("reserved_data", c.c_void_p),
        ("stdin", w.HANDLE), ("stdout", w.HANDLE), ("stderr", w.HANDLE),
    ]


class StartupInfoEx(c.Structure):
    _fields_ = [("info", StartupInfo), ("attributes", c.c_void_p)]


class ProcessInfo(c.Structure):
    _fields_ = [("process", w.HANDLE), ("thread", w.HANDLE),
                ("pid", w.DWORD), ("tid", w.DWORD)]


class SecurityCapabilities(c.Structure):
    _fields_ = [("sid", c.c_void_p), ("capabilities", c.c_void_p),
                ("count", w.DWORD), ("reserved", w.DWORD)]


kernel = c.WinDLL("kernel32", use_last_error=True)
userenv = c.WinDLL("userenv", use_last_error=True)
advapi = c.WinDLL("advapi32", use_last_error=True)
userenv.CreateAppContainerProfile.argtypes = [
    w.LPCWSTR, w.LPCWSTR, w.LPCWSTR, c.c_void_p, w.DWORD, c.POINTER(c.c_void_p)]
userenv.CreateAppContainerProfile.restype = c.c_long
userenv.DeleteAppContainerProfile.argtypes = [w.LPCWSTR]
userenv.DeleteAppContainerProfile.restype = c.c_long
advapi.FreeSid.argtypes = [c.c_void_p]
kernel.InitializeProcThreadAttributeList.argtypes = [
    c.c_void_p, w.DWORD, w.DWORD, c.POINTER(c.c_size_t)]
kernel.UpdateProcThreadAttribute.argtypes = [
    c.c_void_p, w.DWORD, c.c_size_t, c.c_void_p, c.c_size_t, c.c_void_p, c.c_void_p]
kernel.CreateProcessW.argtypes = [
    w.LPCWSTR, w.LPWSTR, c.c_void_p, c.c_void_p, w.BOOL, w.DWORD,
    c.c_void_p, w.LPCWSTR, c.POINTER(StartupInfoEx), c.POINTER(ProcessInfo)]
kernel.WaitForSingleObject.argtypes = [w.HANDLE, w.DWORD]
kernel.GetExitCodeProcess.argtypes = [w.HANDLE, c.POINTER(w.DWORD)]
kernel.CloseHandle.argtypes = [w.HANDLE]
kernel.DeleteProcThreadAttributeList.argtypes = [c.c_void_p]
kernel.TerminateProcess.argtypes = [w.HANDLE, w.UINT]


def require(ok):
    if not ok:
        raise c.WinError(c.get_last_error())


def run(exe, lpac):
    exe = exe.resolve(strict=True)
    root = exe.parent
    # The Rust parent owns and removes this unique staging tree.
    if not root.name.startswith("nospacekey-ipc-integration-"):
        raise ValueError("expected an isolated IPC integration layout")
    subprocess.run([
        "icacls", str(root), "/grant", "*S-1-15-2-1:(OI)(CI)(RX)",
        "*S-1-15-2-2:(OI)(CI)(RX)"], check=True, capture_output=True)
    profile = "nospacekey.test." + uuid.uuid4().hex
    sid = c.c_void_p()
    process = ProcessInfo()
    attributes = None
    initialized = False
    try:
        result = userenv.CreateAppContainerProfile(profile, profile, profile, None, 0, c.byref(sid))
        if result < 0:
            raise OSError(f"CreateAppContainerProfile: {result:#x}")
        size = c.c_size_t()
        count = 2 if lpac else 1
        kernel.InitializeProcThreadAttributeList(None, count, 0, c.byref(size))
        attributes = c.create_string_buffer(size.value)
        require(kernel.InitializeProcThreadAttributeList(attributes, count, 0, c.byref(size)))
        initialized = True
        capabilities = SecurityCapabilities(sid, None, 0, 0)
        require(kernel.UpdateProcThreadAttribute(
            attributes, 0, 0x20009, c.byref(capabilities), c.sizeof(capabilities), None, None))
        opt_out = w.DWORD(1)  # PROCESS_CREATION_ALL_APPLICATION_PACKAGES_OPT_OUT
        if lpac:
            require(kernel.UpdateProcThreadAttribute(
                attributes, 0, 0x2000F, c.byref(opt_out), c.sizeof(opt_out), None, None))
        startup = StartupInfoEx()
        startup.info.cb = c.sizeof(startup)
        startup.attributes = c.cast(attributes, c.c_void_p)
        output = root / "appcontainer-client.log"
        with output.open("wb") as log:
            handle = msvcrt.get_osfhandle(log.fileno())
            os.set_handle_inheritable(handle, True)
            startup.info.flags = 0x100  # STARTF_USESTDHANDLES
            startup.info.stdout = handle
            startup.info.stderr = handle
            command = c.create_unicode_buffer(subprocess.list2cmdline([
                str(exe), "--ignored", "--exact",
                "production_connection_authenticates_a_staged_sibling_engine", "--nocapture"]))
            require(kernel.CreateProcessW(
                str(exe), command, None, None, True, 0x00080000 | 0x08000000,
                None, str(root), c.byref(startup), c.byref(process)))
            if kernel.WaitForSingleObject(process.process, 30000) != 0:
                raise TimeoutError("AppContainer IPC client did not finish")
            status = w.DWORD()
            require(kernel.GetExitCodeProcess(process.process, c.byref(status)))
        print(output.read_text(encoding="utf-8", errors="replace"))
        if status.value:
            raise RuntimeError(f"{'LPAC' if lpac else 'AppContainer'} client exited {status.value}")
        print(f"PASS: {'LPAC' if lpac else 'AppContainer'} authenticated engine connection")
    finally:
        if process.process:
            # This is only the child created above, including timeout cleanup.
            kernel.TerminateProcess(process.process, 1)
            kernel.WaitForSingleObject(process.process, 5000)
            kernel.CloseHandle(process.process)
        if process.thread:
            kernel.CloseHandle(process.thread)
        if initialized:
            kernel.DeleteProcThreadAttributeList(attributes)
        if sid.value:
            result = userenv.DeleteAppContainerProfile(profile)
            advapi.FreeSid(sid)
            if result < 0:
                raise OSError(f"DeleteAppContainerProfile: {result:#x}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("exe", type=Path)
    parser.add_argument("--lpac", action="store_true")
    options = parser.parse_args()
    run(options.exe, options.lpac)
