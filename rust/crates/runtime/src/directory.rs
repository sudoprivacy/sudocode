//! Cross-namespace recipient directory.
//!
//! A [`Mailbox`] is single-root by design: one [`FsBackend`], one `agents_dir`.
//! That is the right shape for reading and writing bytes, and the wrong shape
//! for the one question that spans roots — "who can I reach, across every
//! namespace this session is attached to". A session with both a same-machine
//! pair and a nexus daemon has two namespaces; discovery and delivery must cover
//! both without the model choosing between them.
//!
//! `Directory` is that span, and the ONLY place it lives. `agent_list` enumerates
//! through it and `send` routes through it, so the aggregation rule and the
//! name-collision rule are written once. A single-member directory behaves
//! exactly like the lone mailbox it wraps — the standalone and nexus-only cases
//! are one member, no special path.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::mailbox::Mailbox;

/// A namespace the session can address, paired with the label the model sees.
///
/// The label is the qualifier a caller uses to disambiguate a name that exists
/// in more than one namespace (`local:alice` vs `nexus:alice`). It is a stable
/// `&'static str` because it names a kind of namespace, not an instance.
pub struct Member {
    pub label: &'static str,
    pub mailbox: Arc<Mailbox>,
}

/// An addressable recipient and every namespace it was found in.
///
/// `sources` carries the collision as data rather than resolving it silently:
/// one source is the common case, more than one is a name present in several
/// namespaces, which the caller surfaces so a send can be qualified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipient {
    pub name: String,
    pub sources: Vec<&'static str>,
}

/// Where a `route` lands.
pub enum Routed<'a> {
    /// Exactly one namespace can address the name.
    One(&'a Member),
    /// The name exists in several namespaces and was not qualified; the caller
    /// must re-address with a `label:name` prefix. Carries the labels so the
    /// message can name the choices.
    Ambiguous(Vec<&'static str>),
    /// No namespace has this recipient.
    NotFound,
}

/// An ordered set of addressable namespaces.
///
/// Order is the member order: the first member is the `primary`, used for the
/// session's own identity and for any single-namespace operation that does not
/// name a recipient.
pub struct Directory {
    members: Vec<Member>,
}

impl Directory {
    /// Build from members. The first is the primary; at least one is required
    /// for the directory to have an identity to send as.
    #[must_use]
    pub fn new(members: Vec<Member>) -> Self {
        debug_assert!(!members.is_empty(), "a directory needs at least one member");
        Self { members }
    }

    /// A directory of exactly one namespace — the standalone and nexus-only
    /// shape. Named so the single-member case reads as intent, not a degenerate
    /// list.
    #[must_use]
    pub fn single(label: &'static str, mailbox: Arc<Mailbox>) -> Self {
        Self::new(vec![Member { label, mailbox }])
    }

    /// The primary member — the identity the session sends as.
    #[must_use]
    pub fn primary(&self) -> &Member {
        &self.members[0]
    }

    /// The session's own name, taken from the primary member.
    #[must_use]
    pub fn self_id(&self) -> &str {
        self.members[0].mailbox.self_id()
    }

    /// The mailbox a plain, unqualified operation uses — the primary's.
    #[must_use]
    pub fn primary_mailbox(&self) -> Arc<Mailbox> {
        Arc::clone(&self.members[0].mailbox)
    }

    /// Every addressable recipient across all namespaces, tagged with the
    /// namespaces each was found in.
    ///
    /// Merges by name: a recipient in two namespaces appears once with both
    /// labels in `sources`. An enumeration failure in ANY member is propagated,
    /// not swallowed — "I could not read a namespace" must stay distinct from
    /// "that namespace is empty", the same contract
    /// [`Mailbox::list_recipients`] keeps one level down.
    ///
    /// # Errors
    /// The first member whose namespace cannot be enumerated.
    pub fn list_recipients(&self) -> Result<Vec<Recipient>, String> {
        let mut by_name: BTreeMap<String, Vec<&'static str>> = BTreeMap::new();
        for member in &self.members {
            let own = member.mailbox.self_id();
            for name in member.mailbox.list_recipients()? {
                // An agent is not its own peer. Each namespace reports presence
                // including the session itself; drop it per member so a name that
                // is self in one namespace is still a real peer if another lists it.
                if name == own {
                    continue;
                }
                let sources = by_name.entry(name).or_default();
                if !sources.contains(&member.label) {
                    sources.push(member.label);
                }
            }
        }
        Ok(by_name
            .into_iter()
            .map(|(name, sources)| Recipient { name, sources })
            .collect())
    }

    /// Resolve `to` to the one namespace that should carry the send.
    ///
    /// Accepts a `label:name` qualifier to pick a namespace explicitly; an
    /// unqualified name that exists in exactly one namespace routes there, and
    /// one that exists in several returns [`Routed::Ambiguous`] so the caller
    /// asks for a qualifier rather than guessing. A name found nowhere still
    /// routes to the primary via the caller's own offline-delivery rules — see
    /// `NotFound`, which the caller maps to the primary for durable-inbox sends.
    #[must_use]
    pub fn route(&self, to: &str) -> Routed<'_> {
        if let Some((label, _bare)) = split_qualifier(to) {
            if let Some(member) = self.members.iter().find(|m| m.label == label) {
                return Routed::One(member);
            }
            return Routed::NotFound;
        }
        // One namespace cannot be ambiguous, so do not enumerate it: a send then
        // costs no directory read, and the single-namespace case (standalone or
        // nexus-only) keeps the exact backend-call shape it had before a
        // directory wrapped it. Enumeration happens ONLY to disambiguate a name
        // across several namespaces, which only exists with several members.
        if let [only] = self.members.as_slice() {
            return Routed::One(only);
        }
        let present: Vec<&Member> = self
            .members
            .iter()
            .filter(|m| {
                m.mailbox
                    .list_recipients()
                    .map(|r| r.iter().any(|n| n == to))
                    .unwrap_or(false)
            })
            .collect();
        match present.as_slice() {
            [] => Routed::NotFound,
            [one] => Routed::One(one),
            many => Routed::Ambiguous(many.iter().map(|m| m.label).collect()),
        }
    }

    /// The bare recipient name with any `label:` qualifier stripped — what the
    /// convention turns into a path once the namespace is chosen.
    #[must_use]
    pub fn bare_name(to: &str) -> &str {
        split_qualifier(to).map_or(to, |(_, bare)| bare)
    }
}

/// Split a `label:name` qualifier, if present and the label is a known shape.
///
/// Only splits on a single leading `label:` where `label` is non-empty and
/// contains no path separators — a conversation name with a colon in it is not
/// a qualifier. Returns `(label, bare_name)`.
fn split_qualifier(to: &str) -> Option<(&str, &str)> {
    let (label, bare) = to.split_once(':')?;
    if label.is_empty() || bare.is_empty() || label.contains('/') || label.contains('\\') {
        return None;
    }
    Some((label, bare))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_backend::StdFsBackend;
    use crate::mailbox::InboxConvention;

    fn temp_root(label: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("scode-dir-{label}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A mailbox over a temp root with `peers` provisioned as addressable.
    fn member_with(
        label: &'static str,
        self_id: &str,
        peers: &[&str],
    ) -> (Member, std::path::PathBuf) {
        let root = temp_root(label);
        let mb = Arc::new(Mailbox::new(
            Arc::new(StdFsBackend),
            self_id.to_string(),
            InboxConvention::new(root.to_string_lossy().into_owned()),
        ));
        for p in peers {
            mb.ensure_conversation(p).unwrap();
        }
        (Member { label, mailbox: mb }, root)
    }

    #[test]
    fn single_member_lists_its_peers() {
        let (m, root) = member_with("local", "me", &["alice", "bob"]);
        let dir = Directory::new(vec![m]);
        let names: Vec<String> = dir
            .list_recipients()
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(
            names,
            vec!["alice".to_string(), "bob".to_string()],
            "self is not a peer"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn two_members_merge_and_dedup_with_sources() {
        let (m1, r1) = member_with("local", "me", &["alice", "shared"]);
        let (m2, r2) = member_with("nexus", "me", &["carol", "shared"]);
        let dir = Directory::new(vec![m1, m2]);
        let recips = dir.list_recipients().unwrap();
        let shared = recips.iter().find(|r| r.name == "shared").unwrap();
        assert_eq!(
            shared.sources,
            vec!["local", "nexus"],
            "a name in both namespaces carries both sources"
        );
        let alice = recips.iter().find(|r| r.name == "alice").unwrap();
        assert_eq!(alice.sources, vec!["local"]);
        assert_eq!(recips.len(), 3, "alice + carol + one shared");
        let _ = std::fs::remove_dir_all(r1);
        let _ = std::fs::remove_dir_all(r2);
    }

    #[test]
    fn route_unqualified_single_source_resolves() {
        let (m1, r1) = member_with("local", "me", &["alice"]);
        let (m2, r2) = member_with("nexus", "me", &["carol"]);
        let dir = Directory::new(vec![m1, m2]);
        match dir.route("carol") {
            Routed::One(m) => assert_eq!(m.label, "nexus"),
            _ => panic!("carol is only in nexus"),
        }
        let _ = std::fs::remove_dir_all(r1);
        let _ = std::fs::remove_dir_all(r2);
    }

    #[test]
    fn route_unqualified_multi_source_is_ambiguous() {
        let (m1, r1) = member_with("local", "me", &["shared"]);
        let (m2, r2) = member_with("nexus", "me", &["shared"]);
        let dir = Directory::new(vec![m1, m2]);
        match dir.route("shared") {
            Routed::Ambiguous(labels) => assert_eq!(labels, vec!["local", "nexus"]),
            _ => panic!("shared is in both, must be ambiguous"),
        }
        let _ = std::fs::remove_dir_all(r1);
        let _ = std::fs::remove_dir_all(r2);
    }

    #[test]
    fn route_qualified_picks_the_namespace() {
        let (m1, r1) = member_with("local", "me", &["shared"]);
        let (m2, r2) = member_with("nexus", "me", &["shared"]);
        let dir = Directory::new(vec![m1, m2]);
        match dir.route("nexus:shared") {
            Routed::One(m) => assert_eq!(m.label, "nexus"),
            _ => panic!("qualified route must pick nexus"),
        }
        assert_eq!(Directory::bare_name("nexus:shared"), "shared");
        let _ = std::fs::remove_dir_all(r1);
        let _ = std::fs::remove_dir_all(r2);
    }

    #[test]
    fn route_unknown_in_multi_member_is_not_found() {
        // With several namespaces, a name in none of them is NotFound (the
        // caller then falls back to the primary's durable inbox). A single
        // namespace never reaches here — see `route_single_member_never_enumerates`.
        let (m1, r1) = member_with("local", "me", &["alice"]);
        let (m2, r2) = member_with("nexus", "me", &["carol"]);
        let dir = Directory::new(vec![m1, m2]);
        assert!(matches!(dir.route("nobody"), Routed::NotFound));
        let _ = std::fs::remove_dir_all(r1);
        let _ = std::fs::remove_dir_all(r2);
    }

    #[test]
    fn route_single_member_never_enumerates() {
        // One namespace cannot be ambiguous, so route resolves to it without a
        // directory read — an unknown name included, since its durable inbox is
        // where an offline peer's first message waits.
        let (m1, r1) = member_with("local", "me", &["alice"]);
        let dir = Directory::new(vec![m1]);
        match dir.route("whoever") {
            Routed::One(m) => assert_eq!(m.label, "local"),
            _ => panic!("a single namespace always routes to itself"),
        }
        let _ = std::fs::remove_dir_all(r1);
    }
}
