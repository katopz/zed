//! Fork-owned: threads linked by `continued_from_session_id` (auto_prompt
//! summary handoffs) render as one collapsible group headed by the newest
//! thread, instead of flat rows with `from`/`to` chips.

use super::*;

/// How a thread row takes part in a continuation chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ContinuationRow {
    /// The newest thread of a chain; the older threads fold under it.
    Head {
        /// Identifies the chain across rebuilds: the oldest visible thread,
        /// which stays put while new continuations are added on top.
        root: ThreadId,
        /// The older threads, newest first.
        earlier: Vec<ThreadId>,
        expanded: bool,
    },
    /// An older thread shown indented under its head.
    Child,
}

/// The status a collapsed head shows for its whole chain: the head's own,
/// unless a hidden thread needs attention more (so folding never hides work).
pub(crate) fn chain_status(
    head: AgentThreadStatus,
    hidden: impl IntoIterator<Item = AgentThreadStatus>,
) -> AgentThreadStatus {
    fn urgency(status: AgentThreadStatus) -> u8 {
        match status {
            AgentThreadStatus::WaitingForConfirmation => 4,
            AgentThreadStatus::Running => 3,
            AgentThreadStatus::Error => 2,
            AgentThreadStatus::Unconfirmed => 1,
            AgentThreadStatus::Completed => 0,
        }
    }
    hidden.into_iter().fold(head, |shown, status| {
        match urgency(status) > urgency(shown) {
            true => status,
            false => shown,
        }
    })
}

/// Rows grouped into chains, ready to push.
pub(crate) struct GroupedRows {
    pub visible: Vec<ListEntry>,
    /// Children of collapsed chains: not rendered, but still part of the group.
    pub hidden: Vec<Arc<ThreadEntry>>,
    pub roles: Vec<(ThreadId, ContinuationRow)>,
}

/// Groups `rows` (already in display order) into continuation chains. A chain
/// takes the place of its first row and lists its threads newest first, so the
/// newest thread heads it. A chain is expanded when `expanded` says so, else
/// when `active` is one of its older threads (so the active row never hides).
pub(crate) fn group_continuation_chains(
    rows: Vec<ListEntry>,
    expanded: &HashMap<ThreadId, bool>,
    active: Option<ThreadId>,
    force_expanded: bool,
) -> GroupedRows {
    let session_index: HashMap<&acp::SessionId, usize> = rows
        .iter()
        .enumerate()
        .filter_map(|(ix, row)| match row {
            ListEntry::Thread(thread) => thread.metadata.session_id.as_ref().map(|id| (id, ix)),
            _ => None,
        })
        .collect();
    let parent = |ix: usize| -> Option<usize> {
        let ListEntry::Thread(thread) = &rows[ix] else {
            return None;
        };
        let from = thread.metadata.continued_from_session_id.as_ref()?;
        session_index
            .get(from)
            .copied()
            .filter(|parent| *parent != ix)
    };
    let roots: Vec<usize> = (0..rows.len())
        .map(|ix| {
            let mut current = ix;
            // Bounded walk: a corrupt link cycle must not hang the sidebar.
            for _ in 0..rows.len() {
                match parent(current) {
                    Some(next) => current = next,
                    None => break,
                }
            }
            current
        })
        .collect();
    let root_ids: Vec<Option<ThreadId>> = roots
        .iter()
        .map(|&root| match &rows[root] {
            ListEntry::Thread(thread) => Some(thread.metadata.thread_id),
            _ => None,
        })
        .collect();
    let mut members: HashMap<usize, Vec<usize>> = HashMap::default();
    for (ix, &root) in roots.iter().enumerate() {
        members.entry(root).or_default().push(ix);
    }

    let mut slots: Vec<Option<ListEntry>> = rows.into_iter().map(Some).collect();
    let mut grouped = GroupedRows {
        visible: Vec::new(),
        hidden: Vec::new(),
        roles: Vec::new(),
    };
    for ix in 0..slots.len() {
        let Some(group) = members.remove(&roots[ix]) else {
            continue;
        };
        let (Some(root_id), true) = (root_ids[ix], group.len() > 1) else {
            grouped
                .visible
                .extend(group.iter().filter_map(|&member| slots[member].take()));
            continue;
        };
        let mut threads: Vec<Arc<ThreadEntry>> = group
            .iter()
            .filter_map(|&member| match slots[member].take() {
                Some(ListEntry::Thread(thread)) => Some(thread),
                _ => None,
            })
            .collect();
        threads.sort_by(|left, right| {
            Sidebar::thread_display_time(&right.metadata)
                .cmp(&Sidebar::thread_display_time(&left.metadata))
        });
        let mut threads = threads.into_iter();
        let Some(head) = threads.next() else {
            continue;
        };
        let children: Vec<Arc<ThreadEntry>> = threads.collect();
        let is_expanded = force_expanded
            || expanded.get(&root_id).copied().unwrap_or_else(|| {
                active.is_some_and(|active| {
                    children
                        .iter()
                        .any(|child| child.metadata.thread_id == active)
                })
            });
        grouped.roles.push((
            head.metadata.thread_id,
            ContinuationRow::Head {
                root: root_id,
                earlier: children
                    .iter()
                    .map(|child| child.metadata.thread_id)
                    .collect(),
                expanded: is_expanded,
            },
        ));
        grouped.visible.push(ListEntry::Thread(head));
        for child in children {
            grouped
                .roles
                .push((child.metadata.thread_id, ContinuationRow::Child));
            match is_expanded {
                true => grouped.visible.push(ListEntry::Thread(child)),
                false => grouped.hidden.push(child),
            }
        }
    }
    grouped
}

/// Per-rebuild inputs and outputs of chain grouping, shared by every project group.
pub(crate) struct ChainLayout<'a> {
    pub expanded: &'a HashMap<ThreadId, bool>,
    pub active: Option<ThreadId>,
    pub force_expanded: bool,
    pub rows: &'a mut HashMap<ThreadId, ContinuationRow>,
    pub hidden: &'a mut HashMap<ThreadId, Arc<ThreadEntry>>,
}
