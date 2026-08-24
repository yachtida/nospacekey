//! Windows Toast の identity/payload。送信失敗は呼び出し側が state 更新を行わない。

use crate::release::format_version;
use crate::Version;

pub const AUMID: &str = "yachtida.nospacekey";
pub const TOAST_TAG: &str = "update-available";
pub const TOAST_GROUP: &str = "nospacekey-update";
pub const UPDATE_URI: &str = "nospacekey://update";

pub fn toast_payload(version: &Version) -> String {
    let version = xml_escape(&format_version(version));
    let uri = xml_escape(UPDATE_URI);
    format!(
        r#"<toast activationType="protocol" launch="{uri}"><visual><binding template="ToastGeneric"><text>nospacekey {version} が利用可能です</text></binding></visual><actions><action content="アップデート" activationType="protocol" arguments="{uri}"/></actions></toast>"#
    )
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Notification API の最小 seam。テストでは recording sink を注入できる。
pub trait NotificationSink {
    fn submit(&mut self, payload: &str, tag: &str, group: &str) -> Result<(), String>;
    fn remove_stale(&mut self, tag: &str, group: &str) -> Result<(), String>;
}

#[cfg(windows)]
pub struct WindowsNotificationSink;

#[cfg(windows)]
impl NotificationSink for WindowsNotificationSink {
    fn submit(&mut self, payload: &str, tag: &str, group: &str) -> Result<(), String> {
        let _apartment = WinRtApartment::initialize()?;
        use windows::core::HSTRING;
        use windows::Data::Xml::Dom::XmlDocument;
        use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
        let document = XmlDocument::new().map_err(|error| error.to_string())?;
        document
            .LoadXml(&HSTRING::from(payload))
            .map_err(|error| error.to_string())?;
        let toast = ToastNotification::CreateToastNotification(&document)
            .map_err(|error| error.to_string())?;
        toast
            .SetTag(&HSTRING::from(tag))
            .map_err(|error| error.to_string())?;
        toast
            .SetGroup(&HSTRING::from(group))
            .map_err(|error| error.to_string())?;
        let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID))
            .map_err(|error| error.to_string())?;
        notifier.Show(&toast).map_err(|error| error.to_string())
    }

    fn remove_stale(&mut self, tag: &str, group: &str) -> Result<(), String> {
        let _apartment = WinRtApartment::initialize()?;
        use windows::core::HSTRING;
        use windows::UI::Notifications::ToastNotificationManager;
        ToastNotificationManager::History()
            .map_err(|error| error.to_string())?
            .RemoveGroupedTagWithId(
                &HSTRING::from(tag),
                &HSTRING::from(group),
                &HSTRING::from(AUMID),
            )
            .map_err(|error| error.to_string())
    }
}

#[cfg(windows)]
struct WinRtApartment;

#[cfg(windows)]
impl WinRtApartment {
    fn initialize() -> Result<Self, String> {
        use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
        unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.map_err(|error| error.to_string())?;
        Ok(Self)
    }
}

#[cfg(windows)]
impl Drop for WinRtApartment {
    fn drop(&mut self) {
        unsafe { windows::Win32::System::WinRT::RoUninitialize() };
    }
}

#[cfg(not(windows))]
pub struct WindowsNotificationSink;

#[cfg(not(windows))]
impl NotificationSink for WindowsNotificationSink {
    fn submit(&mut self, _payload: &str, _tag: &str, _group: &str) -> Result<(), String> {
        Err("Windows Toast is unavailable on this platform".into())
    }

    fn remove_stale(&mut self, _tag: &str, _group: &str) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_has_fixed_uri_and_replacement_identity() {
        let version = crate::parse_version("1.2.3-beta.1").unwrap();
        let payload = toast_payload(&version);
        assert!(payload.contains("nospacekey 1.2.3-beta.1 が利用可能です"));
        assert!(payload.starts_with(r#"<toast activationType="protocol""#));
        assert!(payload.contains("activationType=\"protocol\""));
        assert!(payload.contains(UPDATE_URI));
        assert_eq!(TOAST_TAG, "update-available");
        assert_eq!(TOAST_GROUP, "nospacekey-update");
    }

    #[test]
    fn payload_escapes_xml() {
        assert_eq!(xml_escape("a<&\"'"), "a&lt;&amp;&quot;&apos;");
    }
}
