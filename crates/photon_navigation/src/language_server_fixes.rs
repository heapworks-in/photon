/// What a language server needs on the system, and how the user gets it.
#[derive(Debug, PartialEq)]
pub struct KnownFix {
    /// Lowercase language names or extension ids this fix applies to.
    pub languages: &'static [&'static str],
    /// Lowercase fragments of a failure's error text; empty matches any failure.
    pub error_patterns: &'static [&'static str],
    pub hint: &'static str,
    pub command: &'static str,
    pub settings: Option<&'static str>,
}

pub const KNOWN_FIXES: &[KnownFix] = &[
    KnownFix {
        languages: &["java", "gradle", "gradle kts"],
        error_patterns: &[
            "class file version",
            "unsupportedclassversionerror",
            "requires at least java",
            "java executable",
            "unable to locate a java runtime",
        ],
        hint: "jdtls needs Java 21 or newer.",
        command: "brew install openjdk@21",
        settings: Some(
            r#""lsp": { "jdtls": { "settings": { "java_home": "/opt/homebrew/opt/openjdk@21/libexec/openjdk.jdk/Contents/Home" } } }"#,
        ),
    },
    KnownFix {
        languages: &["go"],
        error_patterns: &[],
        hint: "gopls needs Go.",
        command: "brew install go",
        settings: None,
    },
    KnownFix {
        languages: &["ruby"],
        error_patterns: &[],
        hint: "ruby-lsp needs to be installed.",
        command: "gem install ruby-lsp",
        settings: None,
    },
    KnownFix {
        languages: &["c", "c++", "cpp"],
        error_patterns: &[],
        hint: "clangd comes with the Xcode command line tools.",
        command: "xcode-select --install",
        settings: None,
    },
];

/// The fix for `language` (a language name or extension id). With `error`, the fix must also
/// recognise the error text; without one (no server at all), the language alone decides.
pub fn known_fix(language: &str, error: Option<&str>) -> Option<&'static KnownFix> {
    let language = language.to_lowercase();
    let error = error.map(str::to_lowercase);
    KNOWN_FIXES.iter().find(|fix| {
        fix.languages.contains(&language.as_str())
            && error.as_deref().is_none_or(|error| {
                fix.error_patterns.is_empty()
                    || fix
                        .error_patterns
                        .iter()
                        .any(|pattern| error.contains(pattern))
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jdtls_on_an_old_java_gets_the_java_21_fix() {
        let error = "failed to initialize jdtls: Server reset the connection\n-- stderr --\n\
            This version of the Java Runtime only recognizes class file versions up to 61.0";
        let fix = known_fix("Java", Some(error)).expect("a fix");
        assert_eq!(fix.command, "brew install openjdk@21");
        assert!(
            fix.settings
                .is_some_and(|settings| settings.contains("java_home"))
        );
        assert!(known_fix("Java", Some("java.lang.UnsupportedClassVersionError")).is_some());
        assert!(
            known_fix(
                "Gradle",
                Some("Failed to locate Java executable: JAVA_HOME")
            )
            .is_some()
        );
    }

    #[test]
    fn java_errors_with_other_causes_get_no_fix() {
        assert_eq!(known_fix("Java", Some("Failed to download JDTLS")), None);
    }

    #[test]
    fn languages_without_patterns_match_any_failure() {
        assert_eq!(
            known_fix("Go", Some("anything")).map(|fix| fix.command),
            Some("brew install go")
        );
        assert_eq!(
            known_fix("Ruby", Some("anything")).map(|fix| fix.command),
            Some("gem install ruby-lsp")
        );
        assert_eq!(
            known_fix("C++", Some("anything")).map(|fix| fix.command),
            Some("xcode-select --install")
        );
    }

    #[test]
    fn missing_servers_match_by_language_or_extension_id() {
        assert!(known_fix("java", None).is_some());
        assert!(known_fix("C", None).is_some());
        assert_eq!(known_fix("Rust", None), None);
        assert_eq!(known_fix("Rust", Some("class file version")), None);
    }
}
