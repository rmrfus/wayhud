//! CLI specs: an optional leading bare value followed by comma-separated
//! `key=value` fields. Unknown and duplicate fields are rejected.

use anyhow::Result;

/// Split fields at commas outside brackets, preserving CSS values such as
/// `rgb(184, 187, 38)`.
fn split_fields(spec: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (i, c) in spec.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                out.push(&spec[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&spec[start..]);
    out
}

/// Parsed fields are consumed by callers; `finish` rejects leftovers.
/// `asked` tracks supported keys for error messages.
#[derive(Debug)]
pub struct Spec<'a> {
    flag: &'static str,
    bare: Option<&'a str>,
    pairs: Vec<(&'a str, &'a str)>,
    asked: Vec<&'static str>,
}

impl<'a> Spec<'a> {
    pub fn parse(flag: &'static str, spec: &'a str) -> Result<Spec<'a>> {
        anyhow::ensure!(
            !spec.trim().is_empty(),
            "{flag} needs a value, e.g. {flag} 'key=value'"
        );
        let mut bare = None;
        let mut pairs: Vec<(&str, &str)> = Vec::new();
        for (i, item) in split_fields(spec).into_iter().enumerate() {
            let item = item.trim();
            anyhow::ensure!(
                !item.is_empty(),
                "{flag}: empty field in {spec:?}; commas separate fields, \
                 so a trailing or doubled one has nothing to separate"
            );
            match item.split_once('=') {
                Some((key, value)) => {
                    let (key, value) = (key.trim(), value.trim());
                    anyhow::ensure!(!key.is_empty(), "{flag}: {item:?} has no key before the =");
                    anyhow::ensure!(!value.is_empty(), "{flag}: {key} has no value after the =");
                    anyhow::ensure!(
                        !pairs.iter().any(|(seen, _)| *seen == key),
                        "{flag}: {key} given more than once"
                    );
                    pairs.push((key, value));
                }
                // Only the first field may omit its key.
                None => {
                    // Report the pre-1.0 colon separator before it reaches the
                    // colour parser.
                    anyhow::ensure!(
                        !item.contains(':'),
                        "{flag}: {item:?} looks like the old colon form; \
                         fields are now comma-separated, e.g. \
                         {flag} 'value,key=value'"
                    );
                    anyhow::ensure!(
                        i == 0,
                        "{flag}: {item:?} has no key; only the first value may be bare"
                    );
                    bare = Some(item);
                }
            }
        }
        Ok(Spec {
            flag,
            bare,
            pairs,
            asked: Vec::new(),
        })
    }

    /// The leading value, if one was given without a key.
    pub fn bare(&self) -> Option<&'a str> {
        self.bare
    }

    pub fn take(&mut self, key: &'static str) -> Option<&'a str> {
        // Record supported keys even when absent from the spec.
        if !self.asked.contains(&key) {
            self.asked.push(key);
        }
        let i = self.pairs.iter().position(|(k, _)| *k == key)?;
        Some(self.pairs.remove(i).1)
    }

    /// Parse numeric fields with a common error format.
    pub fn take_parsed<T>(&mut self, key: &'static str) -> Result<Option<T>>
    where
        T: std::str::FromStr,
        T::Err: std::fmt::Display,
    {
        let flag = self.flag;
        match self.take(key) {
            None => Ok(None),
            Some(v) => v
                .parse::<T>()
                .map(Some)
                .map_err(|e| anyhow::anyhow!("{flag}: bad value {v:?} for {key} ({e})")),
        }
    }

    /// `true`/`false` only, spelled the way TOML spells them.
    pub fn take_bool(&mut self, key: &'static str) -> Result<Option<bool>> {
        let flag = self.flag;
        match self.take(key) {
            None => Ok(None),
            Some("true") => Ok(Some(true)),
            Some("false") => Ok(Some(false)),
            Some(other) => {
                anyhow::bail!("{flag}: {key} wants true or false, got {other:?}")
            }
        }
    }

    /// Reject unconsumed fields and list supported keys.
    pub fn finish(self) -> Result<()> {
        if let Some((key, _)) = self.pairs.first() {
            anyhow::bail!(
                "{}: unknown field {key:?} (want {})",
                self.flag,
                self.asked.join(", ")
            );
        }
        Ok(())
    }

    /// Read the primary field by name or as the leading bare value.
    /// Reject specs that supply both.
    pub fn headline(&mut self, key: &'static str) -> Result<Option<&'a str>> {
        let named = self.take(key);
        match (self.bare, named) {
            (Some(bare), Some(named)) => anyhow::bail!(
                "{}: {key} given twice, as {bare:?} and as {key}={named:?}",
                self.flag
            ),
            (Some(bare), None) => Ok(Some(bare)),
            (None, named) => Ok(named),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(s: &str) -> Result<Spec<'_>> {
        Spec::parse("--test", s)
    }

    #[test]
    fn a_bare_value_and_pairs_travel_together() {
        let mut s = spec("#b8bb26,radius=12,alpha=0.7").unwrap();
        assert_eq!(s.bare(), Some("#b8bb26"));
        assert_eq!(s.take_parsed::<f64>("radius").unwrap(), Some(12.0));
        assert_eq!(s.take_parsed::<f64>("alpha").unwrap(), Some(0.7));
        s.finish().unwrap();
    }

    #[test]
    fn pairs_alone_and_a_bare_value_alone_both_parse() {
        let mut s = spec("radius=12").unwrap();
        assert_eq!(s.bare(), None);
        assert_eq!(s.take_parsed::<f64>("radius").unwrap(), Some(12.0));

        let s = spec("none").unwrap();
        assert_eq!(s.bare(), Some("none"));
    }

    #[test]
    fn order_of_the_pairs_does_not_matter() {
        let mut a = spec("radius=12,alpha=0.7").unwrap();
        let mut b = spec("alpha=0.7,radius=12").unwrap();
        assert_eq!(
            (
                a.take_parsed::<f64>("radius").unwrap(),
                a.take_parsed::<f64>("alpha").unwrap()
            ),
            (
                b.take_parsed::<f64>("radius").unwrap(),
                b.take_parsed::<f64>("alpha").unwrap()
            )
        );
    }

    #[test]
    fn whitespace_around_fields_is_not_a_typo() {
        let mut s = spec(" #fff , radius = 12 ").unwrap();
        assert_eq!(s.bare(), Some("#fff"));
        assert_eq!(s.take_parsed::<f64>("radius").unwrap(), Some(12.0));
    }

    #[test]
    fn an_unknown_field_is_refused_with_the_ones_that_exist() {
        let mut s = spec("radius=12,radius_px=4").unwrap();

        let _ = s.take("color");
        let _ = s.take_parsed::<f64>("radius").unwrap();
        let _ = s.take_parsed::<f64>("alpha").unwrap();
        let err = format!("{:#}", s.finish().unwrap_err());
        assert!(err.contains("radius_px"), "{err}");
        assert!(
            err.contains("color, radius, alpha"),
            "did not list them: {err}"
        );
    }

    #[test]
    fn a_taken_field_is_not_reported_as_unknown() {
        let mut s = spec("radius=12").unwrap();
        let _ = s.take_parsed::<f64>("radius").unwrap();
        s.finish().unwrap();
    }

    #[test]
    fn the_wanted_list_cannot_contradict_itself() {
        // Supported names must come from the fields the caller consumes.
        let mut s = spec("radius=12").unwrap();
        let _ = s.take("color");
        let err = format!("{:#}", s.finish().unwrap_err());
        assert!(err.contains("unknown field \"radius\""), "{err}");
        assert!(
            !err.contains("want color, radius"),
            "listed the field it just refused: {err}"
        );
    }

    #[test]
    fn a_bare_value_must_come_first() {
        let err = format!("{:#}", spec("radius=12,#fff").unwrap_err());
        assert!(err.contains("only the first value may be bare"), "{err}");
    }

    #[test]
    fn the_same_key_twice_is_refused_rather_than_last_one_wins() {
        let err = format!("{:#}", spec("radius=12,radius=4").unwrap_err());
        assert!(err.contains("more than once"), "{err}");
    }

    #[test]
    fn empty_specs_and_empty_fields_are_refused() {
        for bad in ["", "   ", "a=1,,b=2", "a=1,", ",a=1"] {
            assert!(spec(bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn a_half_written_pair_is_refused() {
        for bad in ["=12", "radius="] {
            assert!(spec(bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn a_bad_number_names_the_field_it_was_for() {
        let mut s = spec("radius=wide").unwrap();
        let err = format!("{:#}", s.take_parsed::<f64>("radius").unwrap_err());
        assert!(err.contains("radius"), "{err}");
        assert!(err.contains("wide"), "{err}");
    }

    #[test]
    fn booleans_are_spelled_the_way_toml_spells_them() {
        let mut s = spec("cursor=false,scroll=true").unwrap();
        assert_eq!(s.take_bool("cursor").unwrap(), Some(false));
        assert_eq!(s.take_bool("scroll").unwrap(), Some(true));

        let mut s = spec("cursor=off").unwrap();
        let err = format!("{:#}", s.take_bool("cursor").unwrap_err());
        assert!(err.contains("true or false"), "{err}");
    }

    #[test]
    fn a_colour_with_commas_in_it_stays_one_field() {
        // CSS function arguments are part of the colour value.
        let mut s = spec("rgb(184, 187, 38),radius=4").unwrap();
        assert_eq!(s.headline("color").unwrap(), Some("rgb(184, 187, 38)"));
        assert_eq!(s.take_parsed::<f64>("radius").unwrap(), Some(4.0));
        s.finish().unwrap();

        let mut s = spec("color=rgba(1,2,3,0.5)").unwrap();
        assert_eq!(s.headline("color").unwrap(), Some("rgba(1,2,3,0.5)"));
    }

    #[test]
    fn the_old_colon_form_says_what_replaced_it() {
        // Report obsolete separators directly.
        let err = format!("{:#}", spec("#b8bb26:12").unwrap_err());
        assert!(err.contains("old colon form"), "{err}");
        let err = format!("{:#}", spec("fade:250").unwrap_err());
        assert!(err.contains("comma-separated"), "{err}");
    }

    #[test]
    fn the_headline_field_reads_the_same_bare_or_named() {
        let mut bare = spec("fade,ms=250").unwrap();
        let mut named = spec("kind=fade,ms=250").unwrap();
        assert_eq!(bare.headline("kind").unwrap(), Some("fade"));
        assert_eq!(named.headline("kind").unwrap(), Some("fade"));
        assert_eq!(bare.take_parsed::<u64>("ms").unwrap(), Some(250));
        assert_eq!(named.take_parsed::<u64>("ms").unwrap(), Some(250));
        bare.finish().unwrap();
        named.finish().unwrap();
    }

    #[test]
    fn the_headline_field_given_both_ways_is_refused() {
        let mut s = spec("#fff,color=#000").unwrap();
        let err = format!("{:#}", s.headline("color").unwrap_err());
        assert!(err.contains("given twice"), "{err}");
    }

    #[test]
    fn a_spec_with_no_headline_leaves_it_unset() {
        // An omitted colour leaves it to the caller to preserve the preset.
        let mut s = spec("radius=12").unwrap();
        assert_eq!(s.headline("color").unwrap(), None);
    }

    #[test]
    fn a_missing_field_is_none_rather_than_an_error() {
        // Omitted fields must remain available for inheritance.
        let mut s = spec("radius=12").unwrap();
        assert_eq!(s.take_parsed::<f64>("alpha").unwrap(), None);
        assert_eq!(s.take_bool("cursor").unwrap(), None);
        assert_eq!(s.take("color"), None);
    }
}
