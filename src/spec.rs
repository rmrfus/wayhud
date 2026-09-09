//! The `key=value` flag grammar, shared by every flag that stands for a config
//! table.
//!
//! One shape everywhere: an optional bare value first, then comma-separated
//! `key=value` pairs, with the keys spelled exactly as the TOML keys are.
//! `--glow '#b8bb26,radius=12'` and `glow = { color = "#b8bb26", radius = 12 }`
//! are the same words in the same order, which is the point — the command line
//! and the config file are one vocabulary rather than two that have to be
//! learned separately.
//!
//! The bare value is the field a flag is usually about: the colour for
//! `--glow`, the kind for `--vanish`. It has to come first, because anywhere
//! else there is no way to tell it from a pair whose key was forgotten.
//!
//! Unknown keys are refused with the list of known ones, the way
//! `deny_unknown_fields` refuses them in the config file. A flag that quietly
//! ignores half of what it was handed is how a keybinding ends up lying about
//! what it does.

use anyhow::Result;

/// Split on the commas that separate fields, leaving alone the ones inside
/// brackets.
///
/// GTK takes `rgb(184, 187, 38)` wherever it takes a colour, and those commas
/// are part of the value. Splitting on every comma cut that into three fields,
/// two of which had no key — a colour that worked before 1.0, when the
/// separator was a colon, and would have started failing without this.
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

/// A parsed spec, emptied field by field and then checked for leftovers.
///
/// Taking rather than reading is what lets `finish` be honest: a key nobody
/// asked for is still sitting in `pairs` at the end, and that is exactly the
/// typo worth reporting.
///
/// `asked` records every key the caller looked for, present or not, so the
/// "want ..." list in that complaint is built from what the flag actually
/// accepts rather than from a literal kept beside it. Those two drifted apart
/// the first time they were written down separately: a field the caller forgot
/// to take was reported as unknown while being listed as known in the same
/// sentence.
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
                // A bare value later in the list is a key someone forgot to
                // type, not a second bare value: say so rather than silently
                // overwrite the first one.
                None => {
                    // A colon where a comma belongs is the pre-1.0 spelling,
                    // which parsed as `colour:size`. Left alone it reaches the
                    // colour parser as one string and comes back "can't parse
                    // RGBA", which tells someone with an old keybinding
                    // nothing about what changed.
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
        // Recorded whether or not it is there: asking is what makes a field
        // part of this flag's vocabulary.
        if !self.asked.contains(&key) {
            self.asked.push(key);
        }
        let i = self.pairs.iter().position(|(k, _)| *k == key)?;
        Some(self.pairs.remove(i).1)
    }

    /// One method for every number in the grammar, so `f64`, `u64` and `usize`
    /// cannot drift into three spellings of the same complaint.
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

    /// Refuse anything left over, listing what this flag does take — the same
    /// answer a typo gets from the config file, where `deny_unknown_fields`
    /// refuses it at load time.
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

    /// The flag's headline field — the colour for `--glow`, the kind for
    /// `--vanish` — however it was written: bare, or by name.
    ///
    /// Both at once is refused rather than resolved. There is no reading of
    /// `--glow '#fff,color=#000'` that is not someone editing a keybinding and
    /// leaving half the old one behind.
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
        // Quoting a spec in a shell makes spaces easy to leave in.
        let mut s = spec(" #fff , radius = 12 ").unwrap();
        assert_eq!(s.bare(), Some("#fff"));
        assert_eq!(s.take_parsed::<f64>("radius").unwrap(), Some(12.0));
    }

    #[test]
    fn an_unknown_field_is_refused_with_the_ones_that_exist() {
        let mut s = spec("radius=12,radius_px=4").unwrap();
        // Exactly what a flag does: ask for each field it supports, whether or
        // not this spec mentioned it.
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
        // The bug this replaces: `finish` took the known names as a literal of
        // their own, so a field the caller forgot to take was reported as
        // unknown AND listed as wanted in the same sentence. Built from what
        // was asked for, the list can no longer say both.
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
        // Otherwise it is a key someone forgot to type, and guessing which
        // field it meant is worse than saying so.
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
        // GTK takes rgb(...) wherever it takes a colour, and those commas are
        // part of the value, not field separators.
        let mut s = spec("rgb(184, 187, 38),radius=4").unwrap();
        assert_eq!(s.headline("color").unwrap(), Some("rgb(184, 187, 38)"));
        assert_eq!(s.take_parsed::<f64>("radius").unwrap(), Some(4.0));
        s.finish().unwrap();

        // And by name, where the value is the part after the first =.
        let mut s = spec("color=rgba(1,2,3,0.5)").unwrap();
        assert_eq!(s.headline("color").unwrap(), Some("rgba(1,2,3,0.5)"));
    }

    #[test]
    fn the_old_colon_form_says_what_replaced_it() {
        // Someone with a pre-1.0 keybinding gets told the separator changed,
        // rather than "can't parse RGBA" from three layers down.
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
        // How "change only the radius" works: nothing was said about the
        // colour, so the caller keeps the preset's.
        let mut s = spec("radius=12").unwrap();
        assert_eq!(s.headline("color").unwrap(), None);
    }

    #[test]
    fn a_missing_field_is_none_rather_than_an_error() {
        // The contract every flag relies on: what the spec does not mention
        // is inherited from the preset instead of reset.
        let mut s = spec("radius=12").unwrap();
        assert_eq!(s.take_parsed::<f64>("alpha").unwrap(), None);
        assert_eq!(s.take_bool("cursor").unwrap(), None);
        assert_eq!(s.take("color"), None);
    }
}
