//! Which monitors a message lands on.
//!
//! Wayland deliberately gives a client no way to ask "which output has the
//! focus" — that is compositor policy, not protocol. So `Current` goes out to
//! sway over its IPC socket and matches the answer back to a `gdk::Monitor` by
//! connector name (`DP-4`), which is the one identifier both sides agree on.

use anyhow::{Result, anyhow};
use gtk::gdk;
use gtk::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputSpec {
    /// The output sway currently has focus on.
    Current,
    /// Every enabled output.
    All,
    /// A specific list of connector names.
    Named(Vec<String>),
}

impl OutputSpec {
    /// Parse the `--output` value: `current`, `all`, or a comma-separated list.
    pub fn parse(s: &str) -> OutputSpec {
        match s {
            "current" => OutputSpec::Current,
            "all" => OutputSpec::All,
            list => OutputSpec::Named(
                list.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect(),
            ),
        }
    }
}

/// Every monitor GDK knows about, in GDK's order.
fn all_monitors(display: &gdk::Display) -> Vec<gdk::Monitor> {
    let list = display.monitors();
    (0..list.n_items())
        .filter_map(|i| list.item(i))
        .filter_map(|obj| obj.downcast::<gdk::Monitor>().ok())
        .collect()
}

/// Ask sway which output is focused. Errors (no `SWAYSOCK`, socket refused)
/// propagate — the caller decides whether to degrade or give up, because
/// "silently used the wrong monitor" is the one outcome nobody wants.
fn sway_focused_output() -> Result<String> {
    let mut conn = swayipc::Connection::new()?;
    conn.get_outputs()?
        .into_iter()
        .find(|o| o.focused)
        .map(|o| o.name)
        .ok_or_else(|| anyhow!("sway reports no focused output"))
}

/// Resolve a spec against the live display.
///
/// A name that matches nothing is reported but not fatal, so
/// `--output DP-3,DP-9` still shows up on DP-3 when DP-9 is unplugged. An
/// empty result IS fatal: showing a HUD on no screen at all is a silent no-op.
pub fn resolve(display: &gdk::Display, spec: &OutputSpec) -> Result<Vec<gdk::Monitor>> {
    let monitors = all_monitors(display);
    if monitors.is_empty() {
        anyhow::bail!("GDK reports no monitors");
    }

    let picked = match spec {
        OutputSpec::All => monitors,
        OutputSpec::Current => {
            let name = sway_focused_output()
                .map_err(|e| anyhow!("--output current needs sway's IPC socket: {e}"))?;
            by_names(&monitors, std::slice::from_ref(&name))
        }
        OutputSpec::Named(names) => by_names(&monitors, names),
    };

    if picked.is_empty() {
        anyhow::bail!("no output matched (have: {})", connector_list(display));
    }
    Ok(picked)
}

fn by_names(monitors: &[gdk::Monitor], names: &[String]) -> Vec<gdk::Monitor> {
    let mut out = Vec::new();
    for name in names {
        let Some(monitor) = monitors
            .iter()
            .find(|m| m.connector().is_some_and(|c| c == *name))
        else {
            eprintln!("wayhud: no output named {name}");
            continue;
        };
        out.push(monitor.clone());
    }
    out
}

fn connector_list(display: &gdk::Display) -> String {
    all_monitors(display)
        .iter()
        .filter_map(|m| m.connector())
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_parsing() {
        assert_eq!(OutputSpec::parse("current"), OutputSpec::Current);
        assert_eq!(OutputSpec::parse("all"), OutputSpec::All);
        assert_eq!(
            OutputSpec::parse("DP-3, eDP-1"),
            OutputSpec::Named(vec!["DP-3".into(), "eDP-1".into()])
        );
    }

    #[test]
    fn empty_names_do_not_become_phantom_outputs() {
        assert_eq!(
            OutputSpec::parse("DP-3,,"),
            OutputSpec::Named(vec!["DP-3".into()])
        );
    }
}
