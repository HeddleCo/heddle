// SPDX-License-Identifier: Apache-2.0
//! Supported language definitions.

/// Supported programming languages.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum Language {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Go,
    C,
    Cpp,
    Java,
    Unknown,
}

impl Language {
    /// Detect language from file extension.
    pub fn from_path(path: &std::path::Path) -> Self {
        match path.extension().and_then(|e| e.to_str()) {
            Some("rs") => Language::Rust,
            Some("py") | Some("pyi") => Language::Python,
            Some("js") | Some("jsx") | Some("mjs") | Some("cjs") => Language::JavaScript,
            Some("ts") | Some("tsx") => Language::TypeScript,
            Some("go") => Language::Go,
            Some("c") | Some("h") => Language::C,
            Some("cpp") | Some("cc") | Some("hpp") | Some("cxx") => Language::Cpp,
            Some("java") => Language::Java,
            _ => Language::Unknown,
        }
    }

    /// Get the tree-sitter language parser. Public mirror of [`parser`]
    /// for consumers outside the `semantic` crate (notably
    /// [`crate::symbol_resolver`] and the `repo` re-export).
    pub fn parser_handle(&self) -> Option<tree_sitter::Language> {
        self.parser()
    }

    /// Get the tree-sitter language parser.
    pub(crate) fn parser(&self) -> Option<tree_sitter::Language> {
        match self {
            #[cfg(feature = "lang-rust")]
            Language::Rust => Some(tree_sitter_rust::LANGUAGE.into()),
            #[cfg(feature = "lang-python")]
            Language::Python => Some(tree_sitter_python::LANGUAGE.into()),
            #[cfg(feature = "lang-javascript")]
            Language::JavaScript => Some(tree_sitter_javascript::LANGUAGE.into()),
            #[cfg(feature = "lang-typescript")]
            Language::TypeScript => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
            #[cfg(feature = "lang-go")]
            Language::Go => Some(tree_sitter_go::LANGUAGE.into()),
            #[cfg(feature = "lang-c")]
            Language::C => Some(tree_sitter_c::LANGUAGE.into()),
            #[cfg(feature = "lang-cpp")]
            Language::Cpp => Some(tree_sitter_cpp::LANGUAGE.into()),
            #[cfg(feature = "lang-java")]
            Language::Java => Some(tree_sitter_java::LANGUAGE.into()),
            _ => None,
        }
    }
}
