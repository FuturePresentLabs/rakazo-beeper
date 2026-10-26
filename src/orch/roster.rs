//! Data-driven fleet roster.
//!
//! The roster is derived from two sources and contains no hardcoded PM list:
//!
//! 1. `verticals.toml` (`PI_PM_VERTICALS`, then `PI_PM_ROOT`/`PI_AGENT_HOME`,
//!    then `~/.pi/agent`) is the canonical vertical list. A vertical opts out of
//!    routing with an explicit `paused = true`; it is still listed so a paused
//!    PM is visible rather than silently dropped.
//! 2. `pi-pm list --tsv` supplies the live pane, phase and worker count. A
//!    vertical with no live PM is still a roster entry, marked offline. A pane
//!    is never invented: a missing pane stays `None`.
//!
//! Stable identities are derived, never configured per entry:
//!
//! - root: `fleet-orch-v1` / `Fleet Orchestrator`
//! - PM: `fleet-pm-<vertical>-v1` / `PM · <Title>`
//!
//! The keys are also the Rakazo server-side idempotency keys, so deriving them
//! must not drift. `title()` keeps existing names stable for the acronyms the
//! fleet already uses (`fpl` -> `FPL`, `ivr` -> `IVR`, …).

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Stable root key; also the Rakazo `spawnKey` for the orchestrator.
pub const ORCH_KEY: &str = "fleet-orch-v1";
/// Stable root display name.
pub const ORCH_NAME: &str = "Fleet Orchestrator";
/// Explicit pane for the root. Optional: without it the root stays unrouted
/// rather than guessing the currently focused tmux pane.
pub const ORCH_PANE_ENV: &str = "RAKAZO_ORCH_PANE";

/// How an entry is matched to an agent and how it may be created.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Created with `bots.create`; our own `spawnKey` is the idempotency key.
    Root,
    /// A real sub-chat under the orchestrator. Only the orchestrator's own
    /// `spawn_bot` tool can set `parentBotId`; no client RPC exposes it.
    Child,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Root => "root",
            Self::Child => "child",
        }
    }
}

/// Live PM state from `pi-pm list --tsv`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Live {
    /// tmux session such as `pm-fpl`.
    pub session: String,
    /// Pane id such as `%40`; `None` when tmux reported no pane.
    pub pane: Option<String>,
    /// Coarse liveness (`live:node`, `stale`, …) as reported by pi-pm.
    pub status: String,
    /// PM phase line.
    pub phase: String,
    /// Live worker count.
    pub workers: u32,
}

/// One roster entry: the stable identity plus its derived live state.
#[derive(Clone, Debug)]
pub struct Entry {
    /// Stable local name; for roots this is the server-side idempotency key.
    pub key: String,
    pub name: String,
    pub title: String,
    pub description: String,
    pub kind: Kind,
    /// An explicit pause in `verticals.toml`. Paused entries are visible but
    /// must never be routed or spawned by accident.
    pub paused: bool,
    /// `None` means no live PM; the entry is offline, not absent.
    pub live: Option<Live>,
}

impl Entry {
    pub fn is_live(&self) -> bool {
        self.live.is_some()
    }

    pub fn pane(&self) -> Option<&str> {
        self.live.as_ref().and_then(|live| live.pane.as_deref())
    }

    /// The vertical name for a PM entry (`fpl` for `fleet-pm-fpl-v1`).
    pub fn vertical(&self) -> Option<&str> {
        match self.kind {
            Kind::Root => None,
            Kind::Child => self
                .key
                .strip_prefix("fleet-pm-")
                .and_then(|rest| rest.strip_suffix("-v1")),
        }
    }
}

/// One `[[vertical]]` block. Unknown fields are ignored on purpose: the PM's
/// file carries fields this binary does not consume (`roots`, `scope`, …).
#[derive(Clone, Debug, Deserialize)]
pub struct Vertical {
    pub name: String,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    /// Explicit opt-out. A paused vertical is listed as paused, never routed.
    #[serde(default)]
    pub paused: bool,
}

#[derive(Debug, Default, Deserialize)]
struct VerticalsFile {
    #[serde(default)]
    vertical: Vec<Vertical>,
}

/// Resolve `verticals.toml` the same way `pi-pm` does.
pub fn verticals_path() -> PathBuf {
    if let Some(path) = std::env::var_os("PI_PM_VERTICALS") {
        return PathBuf::from(path);
    }
    let root = std::env::var_os("PI_PM_ROOT")
        .or_else(|| std::env::var_os("PI_AGENT_HOME"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".pi/agent")))
        .unwrap_or_else(|| PathBuf::from(".pi/agent"));
    root.join("verticals.toml")
}

/// Read and validate one `verticals.toml`.
pub fn load_verticals(path: &Path) -> Result<Vec<Vertical>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read verticals file {}", path.display()))?;
    parse_verticals(&text).with_context(|| format!("invalid verticals file {}", path.display()))
}

/// Parse `verticals.toml` text. Names must be non-empty and unique so a typo
/// cannot create two entries that resolve to the same stable key.
pub fn parse_verticals(text: &str) -> Result<Vec<Vertical>> {
    let file: VerticalsFile = toml::from_str(text)?;
    let mut verticals = file.vertical;
    let mut seen = std::collections::HashSet::new();
    for vertical in &verticals {
        let name = vertical.name.trim();
        ensure!(!name.is_empty(), "vertical with an empty name");
        ensure!(
            name.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')),
            "vertical {name:?} is not a stable slug"
        );
        ensure!(seen.insert(name.to_string()), "duplicate vertical {name:?}");
    }
    verticals.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(verticals)
}

/// Parse `pi-pm list --tsv` output: `vertical session pane live phase workers
/// top unread`. Malformed rows are skipped, not guessed at.
pub fn parse_live_tsv(text: &str) -> BTreeMap<String, Live> {
    let mut live = BTreeMap::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 6 {
            continue;
        }
        let vertical = fields[0].trim();
        if vertical.is_empty() {
            continue;
        }
        let pane = fields[2].trim();
        live.insert(
            vertical.to_string(),
            Live {
                session: fields[1].trim().to_string(),
                pane: (!pane.is_empty() && pane != "-").then(|| pane.to_string()),
                status: fields[3].trim().to_string(),
                phase: fields[4].trim().to_string(),
                workers: fields[5].trim().parse().unwrap_or(0),
            },
        );
    }
    live
}

/// Best-effort live state. A missing `pi-pm` or tmux server means "offline",
/// not an error: the roster still lists every configured vertical.
pub fn live_pms() -> BTreeMap<String, Live> {
    try_live_pms().unwrap_or_default()
}

/// Testable/observable variant of [`live_pms`].
pub fn try_live_pms() -> Result<BTreeMap<String, Live>> {
    let binary = std::env::var_os("PI_PM_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("pi-pm"));
    let output = Command::new(&binary)
        .args(["list", "--tsv"])
        .output()
        .with_context(|| format!("run {} list --tsv", binary.display()))?;
    ensure!(
        output.status.success(),
        "{} list --tsv exited with {}",
        binary.display(),
        output.status
    );
    Ok(parse_live_tsv(&String::from_utf8_lossy(&output.stdout)))
}

/// Root pane, explicitly supplied by the operator. No discovery, no fallback.
pub fn orch_pane() -> Option<String> {
    std::env::var(ORCH_PANE_ENV)
        .ok()
        .map(|pane| pane.trim().to_string())
        .filter(|pane| !pane.is_empty())
}

/// The stable root entry. Always present, never derived from a vertical.
pub fn orchestrator_entry() -> Entry {
    let pane = orch_pane();
    Entry {
        key: ORCH_KEY.to_string(),
        name: ORCH_NAME.to_string(),
        title: "Local fleet orchestrator (pi)".to_string(),
        description: "Runs on the operator's machine. Triages escalations, dispatches fleet workers, and reports merged work.".to_string(),
        kind: Kind::Root,
        paused: false,
        live: pane.map(|pane| Live {
            session: "orch".to_string(),
            pane: Some(pane),
            status: "live:operator".to_string(),
            phase: String::new(),
            workers: 0,
        }),
    }
}

/// Build the roster from an explicit vertical list and live map. No IO, so the
/// "adding a vertical needs no source edit" property is testable offline.
pub fn from_parts(verticals: &[Vertical], live: &BTreeMap<String, Live>) -> Vec<Entry> {
    let mut entries = Vec::with_capacity(verticals.len() + 1);
    entries.push(orchestrator_entry());
    let mut verticals: Vec<&Vertical> = verticals.iter().collect();
    verticals.sort_by(|a, b| a.name.cmp(&b.name));
    for vertical in verticals {
        let name = vertical.name.trim();
        let title = title(name);
        let scope = vertical
            .scope
            .as_deref()
            .map(str::trim)
            .filter(|scope| !scope.is_empty());
        let owner = vertical
            .owner
            .as_deref()
            .map(str::trim)
            .filter(|owner| !owner.is_empty());
        let mut description = format!("Single-threaded PM for the {title} vertical");
        match (owner, scope) {
            (_, Some(scope)) => description.push_str(&format!(" (scope: {scope})")),
            (Some(owner), None) => description.push_str(&format!(" (owner: {owner})")),
            (None, None) => {}
        }
        description.push('.');
        entries.push(Entry {
            key: format!("fleet-pm-{name}-v1"),
            name: format!("PM · {title}"),
            title: format!("{title} vertical PM"),
            description,
            kind: Kind::Child,
            paused: vertical.paused,
            live: live.get(name).cloned(),
        });
    }
    entries
}

/// Production roster: read the configured file and join live PM state.
pub fn load() -> Result<Vec<Entry>> {
    let path = verticals_path();
    let verticals = load_verticals(&path)?;
    Ok(from_parts(&verticals, &live_pms()))
}

/// Display title for a vertical slug: `fpl-cloud` -> `FPL Cloud`.
///
/// Existing fleet PM names must not drift, because a child is resolved by its
/// exact name; the acronym table is therefore part of the identity contract.
pub fn title(vertical: &str) -> String {
    if let Some(fixed) = fixed_title(vertical) {
        return fixed.to_string();
    }
    vertical
        .split(['-', '_', '.'])
        .filter(|token| !token.is_empty())
        .map(|token| {
            if is_acronym(token) {
                token.to_ascii_uppercase()
            } else {
                let mut chars = token.chars();
                match chars.next() {
                    Some(first) => {
                        first.to_ascii_uppercase().to_string()
                            + &chars.as_str().to_ascii_lowercase()
                    }
                    None => String::new(),
                }
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn fixed_title(vertical: &str) -> Option<&'static str> {
    match vertical {
        // Brand casing that title-casing would destroy.
        "ohmos" => Some("ohmOS"),
        _ => None,
    }
}

fn is_acronym(token: &str) -> bool {
    matches!(
        token.to_ascii_lowercase().as_str(),
        "fpl" | "ivr" | "api" | "cli" | "ai" | "ml" | "ui" | "ux" | "os" | "mcp" | "irc" | "dns"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vertical(name: &str, paused: bool) -> Vertical {
        Vertical {
            name: name.to_string(),
            owner: Some(name.to_string()),
            scope: Some(format!("repo:FuturePresentLabs/{name}")),
            paused,
        }
    }

    #[test]
    fn existing_names_and_keys_do_not_drift() {
        let cases = [
            ("agentsmith", "PM · Agentsmith", "fleet-pm-agentsmith-v1"),
            ("edgerunner", "PM · Edgerunner", "fleet-pm-edgerunner-v1"),
            ("fpl", "PM · FPL", "fleet-pm-fpl-v1"),
            ("fpl-cloud", "PM · FPL Cloud", "fleet-pm-fpl-cloud-v1"),
            ("marbles", "PM · Marbles", "fleet-pm-marbles-v1"),
        ];
        for (slug, name, key) in cases {
            assert_eq!(title(slug), name.trim_start_matches("PM · "), "{slug}");
            assert_eq!(format!("PM · {}", title(slug)), name, "{slug}");
            assert_eq!(format!("fleet-pm-{slug}-v1"), key, "{slug}");
        }
    }

    #[test]
    fn added_vertical_needs_no_source_edit() {
        let verticals = parse_verticals(
            r#"
            [[vertical]]
            name = "brand-new"
            owner = "brand-new"
            scope = "repo:FuturePresentLabs/brand-new"
            "#,
        )
        .unwrap();
        let entries = from_parts(&verticals, &BTreeMap::new());
        let added = entries
            .iter()
            .find(|entry| entry.key == "fleet-pm-brand-new-v1")
            .expect("derived entry");
        assert_eq!(added.name, "PM · Brand New");
        assert_eq!(added.kind, Kind::Child);
        assert!(!added.is_live(), "no live row means offline, not absent");
        assert_eq!(added.pane(), None, "a pane is never invented");
    }

    #[test]
    fn live_rows_join_and_offline_verticals_remain_listed() {
        let verticals = vec![vertical("fpl", false), vertical("marbles", false)];
        let live = parse_live_tsv(
            "fpl\tpm-fpl\t%40\tlive:node\treview\t1\tunibus\t0\n\
             marbles\tpm-marbles\t%31\tlive:node\trunning\t2\t-\t1\n",
        );
        let entries = from_parts(&verticals, &live);
        let fpl = entries
            .iter()
            .find(|entry| entry.key == "fleet-pm-fpl-v1")
            .unwrap();
        assert_eq!(fpl.pane(), Some("%40"));
        assert_eq!(fpl.live.as_ref().unwrap().phase, "review");

        // A vertical with no live row is still a roster entry, offline.
        let verticals = vec![vertical("fpl", false), vertical("marbles", false)];
        let entries = from_parts(
            &verticals,
            &parse_live_tsv("fpl\tpm-fpl\t%40\tlive:node\tidle\t0\t-\t0\n"),
        );
        assert_eq!(entries.len(), 3);
        let marbles = entries
            .iter()
            .find(|entry| entry.key == "fleet-pm-marbles-v1")
            .unwrap();
        assert!(!marbles.is_live());
        assert_eq!(marbles.pane(), None);
    }

    #[test]
    fn paused_vertical_is_marked_not_dropped_or_mapped_to_orch() {
        let verticals = vec![vertical("transmog", true), vertical("fpl", false)];
        let entries = from_parts(&verticals, &BTreeMap::new());
        let transmog = entries
            .iter()
            .find(|entry| entry.key == "fleet-pm-transmog-v1")
            .expect("paused vertical stays listed");
        assert!(transmog.paused);
        assert_eq!(transmog.kind, Kind::Child);
        assert!(
            entries
                .iter()
                .all(|entry| entry.key != ORCH_KEY || entry.kind == Kind::Root),
            "paused entry must not become the root"
        );
    }

    #[test]
    fn one_root_and_unique_keys_and_names() {
        let verticals: Vec<_> = ["agentsmith", "fpl", "fpl-cloud", "marbles", "ohmos"]
            .into_iter()
            .map(|name| vertical(name, false))
            .collect();
        let entries = from_parts(&verticals, &BTreeMap::new());
        assert_eq!(entries.iter().filter(|e| e.kind == Kind::Root).count(), 1);
        let keys: std::collections::HashSet<_> = entries.iter().map(|e| &e.key).collect();
        let names: std::collections::HashSet<_> = entries.iter().map(|e| &e.name).collect();
        assert_eq!(keys.len(), entries.len());
        assert_eq!(names.len(), entries.len());
        assert_eq!(entries[0].key, ORCH_KEY);
        assert_eq!(title("ohmos"), "ohmOS");
    }

    #[test]
    fn vertical_parse_rejects_duplicates_and_empty_names() {
        assert!(
            parse_verticals("[[vertical]]\nname = \"fpl\"\n[[vertical]]\nname = \"fpl\"\n")
                .is_err()
        );
        assert!(parse_verticals("[[vertical]]\nname = \"\"\n").is_err());
        assert!(parse_verticals("[[vertical]]\nname = \"has space\"\n").is_err());
    }
}
