use std::{env, fs, path::PathBuf};

fn value(name: &str, default: &str) -> String {
    println!("cargo:rerun-if-env-changed={name}");
    env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn main() {
    let development = env::var_os("CARGO_FEATURE_DEVELOPMENT_CHANNEL").is_some();
    let testing = env::var_os("CARGO_FEATURE_TEST").is_some();
    let slug = value("CVR_SERVICE_SLUG", "clash-verge-service");
    let windows_name = value("CVR_WINDOWS_SERVICE_NAME", "clash_verge_service");
    let display = value("CVR_SERVICE_DISPLAY_NAME", "Clash Verge Service");
    let app_id = value("CVR_APP_ID", "io.github.clash-verge-rev.clash-verge-rev");
    let service_id = value(
        "CVR_MACOS_SERVICE_ID",
        "io.github.clash-verge-rev.clash-verge-rev.service",
    );
    let channel_slug = if development {
        format!("{slug}-dev")
    } else {
        slug.clone()
    };
    let channel_windows = if development {
        format!("{windows_name}_dev")
    } else {
        windows_name.clone()
    };
    let channel_app_id = if development {
        format!("{app_id}.dev")
    } else {
        app_id.clone()
    };
    let channel_service_id = if development {
        if service_id == format!("{app_id}.service") {
            format!("{app_id}.dev.service")
        } else {
            format!("{service_id}.dev")
        }
    } else {
        service_id.clone()
    };
    let channel_display = if development {
        display
            .strip_suffix(" Service")
            .map(|app| format!("{app} Development Service"))
            .unwrap_or_else(|| format!("{display} Development"))
    } else {
        display
    };
    let namespace = if testing {
        format!("{channel_slug}-ipc-test")
    } else {
        channel_slug.clone()
    };
    let target = env::var("CARGO_CFG_TARGET_OS").unwrap();
    let ipc_path = if target == "windows" {
        let pipe = if testing {
            format!("{channel_slug}-test")
        } else {
            channel_slug.clone()
        };
        format!(r"\\.\pipe\{pipe}")
    } else if testing {
        format!("/tmp/{namespace}/service.sock")
    } else {
        let root = if target == "macos" { "/var/run" } else { "/run" };
        format!("{root}/{channel_slug}/service.sock")
    };
    let lock_name = value("CVR_CORE_LOCK_NAME", "clash-verge-service.core-execution");
    let lock_name = if testing {
        format!("{lock_name}-test")
    } else {
        lock_name
    };
    let constants = [
        ("CORE_NAME", value("CVR_CORE_NAME", "verge-mihomo")),
        ("CORE_ALPHA_NAME", value("CVR_CORE_ALPHA_NAME", "verge-mihomo-alpha")),
        ("CORE_IPC_NAME", value("CVR_CORE_IPC_NAME", "verge-mihomo")),
        ("SERVICE_BINARY", value("CVR_SERVICE_BINARY", "clash-verge-service")),
        (
            "OWNER_TOKEN_FILE_NAME",
            value("CVR_OWNER_TOKEN_FILE", ".clash-verge-service-owner-token"),
        ),
        ("CORE_LOCK_NAME", lock_name.clone()),
        ("CORE_MUTEX_NAME", format!(r"Global\{lock_name}")),
        ("IPC_PATH", ipc_path),
        ("TEST_STATE_NAMESPACE", format!("{namespace}-state")),
        (
            "OTHER_SERVICE_SLUG",
            if development {
                slug
            } else {
                format!("{channel_slug}-dev")
            },
        ),
        (
            "OTHER_WINDOWS_SERVICE_NAME",
            if development {
                windows_name
            } else {
                format!("{channel_windows}_dev")
            },
        ),
        (
            "OTHER_MACOS_SERVICE_ID",
            if development {
                service_id
            } else if service_id == format!("{app_id}.service") {
                format!("{app_id}.dev.service")
            } else {
                format!("{service_id}.dev")
            },
        ),
    ];
    let custom = value("CVR_CUSTOM_BUILD", "0") == "1";
    let mut source = format!("pub const CUSTOM_BUILD: bool = {custom};\n");
    for (name, value) in constants {
        let configuration = match name {
            "CORE_LOCK_NAME" => "#[cfg(any(feature = \"client\", feature = \"standalone\"))]\n",
            "CORE_MUTEX_NAME" | "OTHER_WINDOWS_SERVICE_NAME" => {
                "#[cfg(all(windows, any(feature = \"client\", feature = \"standalone\")))]\n"
            }
            "OTHER_MACOS_SERVICE_ID" => "#[cfg(all(target_os = \"macos\", feature = \"client\"))]\n",
            "OTHER_SERVICE_SLUG" => "#[cfg(all(target_os = \"linux\", feature = \"client\"))]\n",
            "TEST_STATE_NAMESPACE" => "#[cfg(feature = \"test\")]\n",
            _ => "",
        };
        source.push_str(&format!("{configuration}pub const {name}: &str = {value:?};\n"));
    }
    let id = if development { "development" } else { "production" };
    source.push_str(&format!("pub const CHANNEL_IDENTITY: ChannelIdentity = ChannelIdentity {{ id: {id:?}, service_slug: {channel_slug:?}, windows_service_name: {channel_windows:?}, service_display_name: {channel_display:?}, macos_app_bundle_id: {channel_app_id:?}, macos_service_id: {channel_service_id:?} }};\n"));
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("identity.rs"),
        source,
    )
    .unwrap();
}
