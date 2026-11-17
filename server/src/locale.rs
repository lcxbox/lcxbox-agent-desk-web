//! Process-wide locale state shared by HTTP handlers, the daemon and workers.
//!
//! `Settings.system.locale` is the durable source of truth. This module owns
//! only the live process value and the mapping from the browser's BCP-47 tags
//! to the locale identifiers used by the Rust translation catalog.

pub const DEFAULT_LOCALE: &str = "zh-CN";
pub const SUPPORTED_LOCALES: [&str; 2] = ["en-US", "zh-CN"];

#[cfg(not(test))]
static GLOBAL_LOCALE: once_cell::sync::Lazy<std::sync::RwLock<String>> =
    once_cell::sync::Lazy::new(|| std::sync::RwLock::new(DEFAULT_LOCALE.to_string()));

#[cfg(not(test))]
fn store(locale: &str) {
    *GLOBAL_LOCALE.write().unwrap() = locale.to_string();
}

#[cfg(not(test))]
fn load() -> String {
    GLOBAL_LOCALE.read().unwrap().clone()
}

// Tests run in parallel threads of one process; each thread keeps its own
// value so a test never observes a locale another test applied.
#[cfg(test)]
thread_local! {
    static TEST_LOCALE: std::cell::RefCell<String> =
        std::cell::RefCell::new(DEFAULT_LOCALE.to_string());
}

#[cfg(test)]
fn store(locale: &str) {
    TEST_LOCALE.with(|value| *value.borrow_mut() = locale.to_string());
}

#[cfg(test)]
fn load() -> String {
    TEST_LOCALE.with(|value| value.borrow().clone())
}

/// Return the canonical locale accepted by both the web UI and native shell.
pub fn canonicalize(locale: &str) -> Option<&'static str> {
    match locale.trim().to_ascii_lowercase().as_str() {
        "en" | "en-us" | "en_us" => Some("en-US"),
        "zh" | "zh-cn" | "zh_cn" | "zh-hans" => Some("zh-CN"),
        _ => None,
    }
}

/// Apply a locale to the current process and return its canonical BCP-47 tag.
pub fn set_global_locale(locale: &str) -> Result<&'static str, String> {
    let canonical = canonicalize(locale).ok_or_else(|| {
        format!(
            "unsupported locale {locale:?}; supported locales: {}",
            SUPPORTED_LOCALES.join(", ")
        )
    })?;
    let rust_locale = match canonical {
        "en-US" => "en",
        "zh-CN" => "zh-CN",
        _ => unreachable!("canonicalize only returns supported locales"),
    };
    rust_i18n::set_locale(rust_locale);
    store(canonical);
    Ok(canonical)
}

pub fn current_locale() -> String {
    load()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_are_normalized_to_web_locale_tags() {
        assert_eq!(canonicalize("en"), Some("en-US"));
        assert_eq!(canonicalize(" EN_us "), Some("en-US"));
        assert_eq!(canonicalize("zh-Hans"), Some("zh-CN"));
        assert_eq!(canonicalize("fr-FR"), None);
    }

    #[test]
    fn applying_locale_updates_the_process_value() {
        let previous = current_locale();
        assert_eq!(set_global_locale("en"), Ok("en-US"));
        assert_eq!(current_locale(), "en-US");
        let _ = set_global_locale(&previous);
    }
}
