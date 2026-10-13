#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelIdentity {
    pub id: &'static str,
    pub service_slug: &'static str,
    pub windows_service_name: &'static str,
    pub service_display_name: &'static str,
    pub macos_app_bundle_id: &'static str,
    pub macos_service_id: &'static str,
}

include!(concat!(env!("OUT_DIR"), "/identity.rs"));

pub const SERVICE_SLUG: &str = CHANNEL_IDENTITY.service_slug;
pub const WINDOWS_SERVICE_NAME: &str = CHANNEL_IDENTITY.windows_service_name;
pub const SERVICE_DISPLAY_NAME: &str = CHANNEL_IDENTITY.service_display_name;
pub const MACOS_APP_BUNDLE_ID: &str = CHANNEL_IDENTITY.macos_app_bundle_id;
pub const MACOS_SERVICE_ID: &str = CHANNEL_IDENTITY.macos_service_id;

#[cfg(test)]
mod tests {
    use super::CHANNEL_IDENTITY;

    #[test]
    fn configured_service_identity_is_registered() {
        if let Some(expected) = option_env!("CVR_SERVICE_SLUG") {
            let expected = if cfg!(feature = "development-channel") {
                format!("{expected}-dev")
            } else {
                expected.to_owned()
            };
            assert_eq!(CHANNEL_IDENTITY.service_slug, expected);
        }
        if let Some(expected) = option_env!("CVR_WINDOWS_SERVICE_NAME") {
            let expected = if cfg!(feature = "development-channel") {
                format!("{expected}_dev")
            } else {
                expected.to_owned()
            };
            assert_eq!(CHANNEL_IDENTITY.windows_service_name, expected);
        }
        if let Some(expected) = option_env!("CVR_OWNER_TOKEN_FILE") {
            assert_eq!(crate::OWNER_TOKEN_FILE_NAME, expected);
        }
    }

    #[test]
    fn compiled_channel_has_a_self_consistent_identity() {
        assert!(!CHANNEL_IDENTITY.id.is_empty());
        assert!(!CHANNEL_IDENTITY.service_slug.is_empty());
        assert!(!CHANNEL_IDENTITY.windows_service_name.is_empty());
        assert!(!CHANNEL_IDENTITY.macos_service_id.is_empty());
    }
}
