//! Background streaming needs credentials after the device locks. Keep the
//! accessibility policy in the iOS host and retain the existing item identity.
use amp_collector::{ConnectionKey, EnrolledCredential, NativeCredentialStorage, ids};
use core_foundation::{base::TCFType, data::CFData, dictionary::CFDictionary, string::CFString};
use security_framework::{base::Error, passwords};
use security_framework_sys::{
    access_control::kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly,
    base::{errSecDuplicateItem, errSecItemNotFound},
    item::{kSecAttrAccount, kSecAttrService, kSecClass, kSecClassGenericPassword, kSecValueData},
    keychain_item::{SecItemAdd, SecItemUpdate},
};

// The sys crate exposes the protection values but omits their attribute key.
#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    static kSecAttrAccessible: core_foundation::string::CFStringRef;
}

pub struct Credentials;
const SERVICE: &str = "trash-amp.collector";
fn account(key: ConnectionKey) -> String {
    format!("{}/{}", key.server_id, key.user_id)
}
fn unavailable(_: impl std::fmt::Display) -> ids::Error {
    ids::Error::Unavailable("Could not access Collector credentials in Keychain".into())
}
impl NativeCredentialStorage for Credentials {
    fn save(&self, value: &EnrolledCredential) -> ids::Result<()> {
        let key = ConnectionKey {
            server_id: value.server_id,
            user_id: value.user_id.ok_or(ids::Error::Unauthenticated)?,
        };
        let bytes = serde_json::to_vec(value).map_err(unavailable)?;
        // SAFETY: All CF values are owned for the duration of these synchronous
        // calls. Query only by identity; protection belongs in the update, so
        // existing WhenUnlocked items are migrated atomically without deletion.
        unsafe {
            let query = vec![
                (
                    CFString::wrap_under_get_rule(kSecClass),
                    CFString::wrap_under_get_rule(kSecClassGenericPassword).into_CFType(),
                ),
                (
                    CFString::wrap_under_get_rule(kSecAttrService),
                    CFString::new(SERVICE).into_CFType(),
                ),
                (
                    CFString::wrap_under_get_rule(kSecAttrAccount),
                    CFString::new(&account(key)).into_CFType(),
                ),
            ];
            let attributes = vec![
                (
                    CFString::wrap_under_get_rule(kSecAttrAccessible),
                    CFString::wrap_under_get_rule(kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly)
                        .into_CFType(),
                ),
                (
                    CFString::wrap_under_get_rule(kSecValueData),
                    CFData::from_buffer(&bytes).into_CFType(),
                ),
            ];
            let query_dict = CFDictionary::from_CFType_pairs(&query);
            let attributes_dict = CFDictionary::from_CFType_pairs(&attributes);
            let mut status = SecItemUpdate(
                query_dict.as_concrete_TypeRef(),
                attributes_dict.as_concrete_TypeRef(),
            );
            if status == errSecItemNotFound {
                let mut item = query;
                item.extend(attributes);
                status = SecItemAdd(
                    CFDictionary::from_CFType_pairs(&item).as_concrete_TypeRef(),
                    std::ptr::null_mut(),
                );
                if status == errSecDuplicateItem {
                    status = SecItemUpdate(
                        query_dict.as_concrete_TypeRef(),
                        attributes_dict.as_concrete_TypeRef(),
                    );
                }
            }
            if status == 0 {
                Ok(())
            } else {
                Err(unavailable(Error::from_code(status)))
            }
        }
    }
    fn load(&self, key: ConnectionKey) -> ids::Result<Option<EnrolledCredential>> {
        match passwords::get_generic_password(SERVICE, &account(key)) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(unavailable),
            Err(error) if error.code() == errSecItemNotFound => Ok(None),
            Err(error) => Err(unavailable(error)),
        }
    }
    fn remove(&self, key: ConnectionKey) -> ids::Result<()> {
        match passwords::delete_generic_password(SERVICE, &account(key)) {
            Ok(()) => Ok(()),
            Err(error) if error.code() == errSecItemNotFound => Ok(()),
            Err(error) => Err(unavailable(error)),
        }
    }
    fn load_legacy(&self, _: ids::ServerId) -> ids::Result<Option<EnrolledCredential>> {
        Ok(None)
    }
    fn remove_legacy(&self, _: ids::ServerId) -> ids::Result<()> {
        Ok(())
    }
}
