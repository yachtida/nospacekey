import WinSDK

/// AppContainer clients must be able to authenticate the image behind the pipe.
/// Preserve the process DACL and grant only the right QueryFullProcessImageName
/// needs; never grant memory access, handle duplication, or process mutation.
func allowEngineImageQueries() -> Bool {
    let process = GetCurrentProcess()
    var descriptor: PSECURITY_DESCRIPTOR? = nil
    var existing: PACL? = nil
    guard GetSecurityInfo(process, SE_KERNEL_OBJECT, DWORD(DACL_SECURITY_INFORMATION),
                          nil, nil, &existing, nil, &descriptor) == DWORD(ERROR_SUCCESS),
          let descriptor else { return false }
    defer { LocalFree(descriptor) }
    guard existing != nil else { return false }

    var packages: PSID? = nil
    var restrictedPackages: PSID? = nil
    guard "S-1-15-2-1".withCString(encodedAs: UTF16.self, {
        ConvertStringSidToSidW($0, &packages) != false
    }), let packages else { return false }
    defer { LocalFree(packages) }
    guard "S-1-15-2-2".withCString(encodedAs: UTF16.self, {
        ConvertStringSidToSidW($0, &restrictedPackages) != false
    }), let restrictedPackages else { return false }
    defer { LocalFree(restrictedPackages) }

    var entries = [EXPLICIT_ACCESS_W](repeating: EXPLICIT_ACCESS_W(), count: 2)
    for (index, sid) in [packages, restrictedPackages].enumerated() {
        entries[index].grfAccessPermissions = DWORD(PROCESS_QUERY_LIMITED_INFORMATION)
        entries[index].grfAccessMode = GRANT_ACCESS
        entries[index].grfInheritance = DWORD(NO_INHERITANCE)
        entries[index].Trustee.TrusteeForm = TRUSTEE_IS_SID
        entries[index].Trustee.TrusteeType = TRUSTEE_IS_WELL_KNOWN_GROUP
        entries[index].Trustee.ptstrName = sid.assumingMemoryBound(to: WCHAR.self)
    }
    var updated: PACL? = nil
    guard SetEntriesInAclW(ULONG(entries.count), &entries, existing, &updated) == DWORD(ERROR_SUCCESS),
          let updated else { return false }
    defer { LocalFree(updated) }
    return SetSecurityInfo(process, SE_KERNEL_OBJECT, DWORD(DACL_SECURITY_INFORMATION),
                           nil, nil, updated, nil) == DWORD(ERROR_SUCCESS)
}
