//! Which plugin handles which file.
//!
//! Hearth itself knows nothing about text, tables or images. It knows how to
//! find the plugin that does, and everything else — reading, writing,
//! hashing, provenance, migration — belongs to the spine. Adding a sixth
//! format means adding one line to [`all`].

use libwick::plugin::Plugin;
use libwick::{Error, Result, Tag};

pub fn all() -> Vec<Box<dyn Plugin>> {
    vec![
        Box::new(wick_emt::Emt),
        Box::new(wick_emd::Emd),
        Box::new(wick_emi::Emi),
        Box::new(wick_emc::Emc),
        Box::new(wick_emx::Emx),
    ]
}

pub struct Registry {
    plugins: Vec<Box<dyn Plugin>>,
}

impl Default for Registry {
    fn default() -> Self {
        Registry::new()
    }
}

impl Registry {
    pub fn new() -> Self {
        Registry { plugins: all() }
    }

    pub fn iter(&self) -> impl Iterator<Item = &dyn Plugin> {
        self.plugins.iter().map(|p| p.as_ref())
    }

    pub fn by_tag(&self, tag: Tag) -> Result<&dyn Plugin> {
        self.iter().find(|p| p.tag() == tag).ok_or_else(|| {
            Error::Other(format!(
                "no plugin for format tag '{tag}'. This build handles: {}",
                self.iter()
                    .map(|p| format!("{} (.{})", p.tag(), p.ext()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })
    }

    pub fn by_ext(&self, ext: &str) -> Result<&dyn Plugin> {
        self.iter()
            .find(|p| p.ext() == ext)
            .ok_or_else(|| Error::Other(format!("'.{ext}' is not an Ember format")))
    }

    /// Every plugin that can import this legacy extension.
    pub fn importers(&self, ext: &str) -> Vec<&dyn Plugin> {
        self.iter().filter(|p| p.imports().contains(&ext)).collect()
    }

    /// The plugin to use for a legacy extension when the user has not said.
    ///
    /// Several extensions are legitimately claimed by two plugins, and the
    /// choice is a real one rather than an accident:
    ///
    /// * `.md` and `.txt` go to `.emt`, because `.emt` round-trips them byte
    ///   for byte. `.emd` is the right answer when the destination is a
    ///   laid-out document, and `--as emd` says so.
    /// * `.json` goes to `.emc`, because most JSON is configuration. Tabular
    ///   JSON is `--as emx`.
    ///
    /// Ambiguity is resolved silently but reported, so nobody discovers the
    /// default by finding the wrong file on disk later.
    pub fn preferred(&self, ext: &str) -> Result<(&dyn Plugin, Vec<&'static str>)> {
        let candidates = self.importers(ext);
        if candidates.is_empty() {
            return Err(Error::Other(format!(
                "nothing imports '.{ext}'. Supported: {}",
                self.legacy_extensions().join(", ")
            )));
        }
        let preferred = match ext {
            "txt" | "text" | "md" | "markdown" | "log" | "rst" => "emt",
            "json" | "yaml" | "yml" | "toml" => "emc",
            "csv" | "tsv" => "emx",
            "png" => "emi",
            "pdf" => "emd",
            _ => candidates[0].ext(),
        };
        let chosen = candidates
            .iter()
            .find(|p| p.ext() == preferred)
            .copied()
            .unwrap_or(candidates[0]);
        let others = candidates
            .iter()
            .filter(|p| p.ext() != chosen.ext())
            .map(|p| p.ext())
            .collect();
        Ok((chosen, others))
    }

    pub fn legacy_extensions(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .iter()
            .flat_map(|p| p.imports().iter().map(|e| format!(".{e}")))
            .collect();
        v.sort();
        v.dedup();
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_plugin_has_a_distinct_tag_and_extension() {
        let r = Registry::new();
        let mut tags: Vec<String> = r.iter().map(|p| p.tag().to_string()).collect();
        let mut exts: Vec<&str> = r.iter().map(|p| p.ext()).collect();
        let (n, m) = (tags.len(), exts.len());
        tags.sort();
        tags.dedup();
        exts.sort();
        exts.dedup();
        assert_eq!(tags.len(), n, "two plugins share a format tag");
        assert_eq!(exts.len(), m, "two plugins share an extension");
    }

    #[test]
    fn ambiguous_extensions_resolve_and_report_the_alternatives() {
        let r = Registry::new();
        let (p, others) = r.preferred("md").unwrap();
        assert_eq!(p.ext(), "emt");
        assert_eq!(others, vec!["emd"]);

        let (p, _) = r.preferred("json").unwrap();
        assert_eq!(p.ext(), "emc");
        let (p, _) = r.preferred("csv").unwrap();
        assert_eq!(p.ext(), "emx");
    }

    #[test]
    fn an_unknown_extension_lists_what_is_supported() {
        let err = match Registry::new().preferred("docx") {
            Err(e) => e.to_string(),
            Ok(_) => panic!("an unknown extension was accepted"),
        };
        assert!(err.contains("nothing imports '.docx'"));
        assert!(err.contains(".csv"), "{err}");
    }

    #[test]
    fn lookup_by_tag_and_extension_agree() {
        let r = Registry::new();
        for p in r.iter() {
            assert_eq!(r.by_tag(p.tag()).unwrap().ext(), p.ext());
            assert_eq!(r.by_ext(p.ext()).unwrap().tag(), p.tag());
        }
    }
}
